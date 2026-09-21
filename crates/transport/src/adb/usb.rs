use std::sync::Arc;

use nusb::transfer::{EndpointType, RequestBuffer};
use tokio::sync::Mutex;

use super::io::AdbIo;
use super::protocol::AdbError;

const ADB_CLASS: u8 = 0xff;
const ADB_SUBCLASS: u8 = 0x42;
const ADB_PROTOCOL: u8 = 0x01;

pub struct UsbIo {
    iface: nusb::Interface,
    ep_in: u8,
    ep_out: u8,
    mps_in: usize,
    mps_out: usize,
    pending: Mutex<Vec<u8>>,
    write_lock: Mutex<()>,
}

pub fn adb_serials() -> Result<Vec<String>, AdbError> {
    let mut out = Vec::new();
    for dev in list_adb()? {
        if let Some(serial) = dev.serial_number() {
            out.push(serial.to_string());
        } else {
            out.push(format!("{:04x}:{:04x}", dev.vendor_id(), dev.product_id()));
        }
    }
    Ok(out)
}

pub async fn open_usb(serial: Option<&str>) -> Result<Arc<dyn AdbIo>, AdbError> {
    let mut found = Vec::new();
    for dev in list_adb()? {
        let matches_serial = match serial {
            None => true,
            Some(want) => dev.serial_number() == Some(want),
        };
        if matches_serial {
            found.push(dev);
        }
    }
    if found.is_empty() {
        return Err(AdbError::Usb(
            "no ADB interface (class 0xff/0x42/0x01). Is USB debugging on?".into(),
        ));
    }
    if serial.is_none() && found.len() > 1 {
        let list = found
            .iter()
            .map(|d| d.serial_number().unwrap_or("?").to_string())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(AdbError::Usb(format!(
            "multiple devices ({list}); pass --serial"
        )));
    }
    let info = found.remove(0);
    let iface_num = info
        .interfaces()
        .find(|i| is_adb(i.class(), i.subclass(), i.protocol()))
        .map(|i| i.interface_number())
        .ok_or_else(|| AdbError::Usb("ADB interface disappeared".into()))?;

    let device = info
        .open()
        .map_err(|e| AdbError::Usb(format!("open: {e}")))?;
    // macOS does not configure a vendor-class device by itself. With no
    // configuration, bulk pipes do not exist and transfers fail as "unknown error".
    if device.active_configuration().is_err() {
        let value = device
            .configurations()
            .next()
            .map(|c| c.configuration_value())
            .unwrap_or(1);
        log::info!("usb device is unconfigured; setting configuration {value}");
        device.set_configuration(value).map_err(|e| {
            AdbError::Usb(format!("set configuration {value}: {e}"))
        })?;
    }
    let interface = device
        .claim_interface(iface_num)
        .map_err(|e| AdbError::Usb(format!("claim interface {iface_num}: {e}")))?;
    // Do not call set_alt_setting(0). On macOS, SetAlternateInterface(0) when
    // the interface is already on alternate 0 makes the next bulk OUT complete
    // with kIOReturnIsoTooNew (0xe00002ed), which nusb reports as "unknown error".

    let mut ep_in = None;
    let mut ep_out = None;
    let mut mps_in = 512usize;
    let mut mps_out = 512usize;
    if let Some(alt) = interface.descriptors().next() {
        for ep in alt.endpoints() {
            if ep.transfer_type() != EndpointType::Bulk {
                continue;
            }
            let mps = ep.max_packet_size().max(1);
            if ep.address() & 0x80 != 0 {
                ep_in = Some(ep.address());
                mps_in = mps;
            } else {
                ep_out = Some(ep.address());
                mps_out = mps;
            }
        }
    }
    let ep_in = ep_in.ok_or_else(|| AdbError::Usb("no bulk IN endpoint".into()))?;
    let ep_out = ep_out.ok_or_else(|| AdbError::Usb("no bulk OUT endpoint".into()))?;
    log::info!("usb adb iface {iface_num} in {ep_in:#04x}/{mps_in} out {ep_out:#04x}/{mps_out}");
    // The previous adb process may have left the pipes stalled.
    let _ = interface.clear_halt(ep_in);
    let _ = interface.clear_halt(ep_out);

    let io: Arc<dyn AdbIo> = Arc::new(UsbIo {
        iface: interface,
        ep_in,
        ep_out,
        mps_in,
        mps_out,
        pending: Mutex::new(Vec::new()),
        write_lock: Mutex::new(()),
    });
    Ok(io)
}

fn list_adb() -> Result<Vec<nusb::DeviceInfo>, AdbError> {
    let iter = nusb::list_devices().map_err(|e| AdbError::Usb(e.to_string()))?;
    Ok(iter
        .filter(|d| {
            d.interfaces()
                .any(|i| is_adb(i.class(), i.subclass(), i.protocol()))
        })
        .collect())
}

fn is_adb(class: u8, subclass: u8, protocol: u8) -> bool {
    class == ADB_CLASS && subclass == ADB_SUBCLASS && protocol == ADB_PROTOCOL
}

#[async_trait::async_trait]
impl AdbIo for UsbIo {
    async fn read_exact(&self, buf: &mut [u8]) -> Result<(), AdbError> {
        let mut pending = self.pending.lock().await;
        let mut filled = 0;
        while filled < buf.len() {
            if pending.is_empty() {
                let want = round_up((buf.len() - filled).max(self.mps_in), self.mps_in)
                    .max(self.mps_in);
                let want = want.min(16 * 1024).max(self.mps_in);
                let want = round_up(want, self.mps_in);
                let data = self
                    .iface
                    .bulk_in(self.ep_in, RequestBuffer::new(want))
                    .await
                    .into_result()
                    .map_err(|e| {
                        AdbError::Usb(format!("bulk in {:#04x}: {e}", self.ep_in))
                    })?;
                if data.is_empty() {
                    return Err(AdbError::Usb("zero-length IN".into()));
                }
                pending.extend_from_slice(&data);
            }
            let n = (buf.len() - filled).min(pending.len());
            buf[filled..filled + n].copy_from_slice(&pending[..n]);
            pending.drain(..n);
            filled += n;
        }
        Ok(())
    }

    async fn write_all(&self, buf: &[u8]) -> Result<(), AdbError> {
        let _guard = self.write_lock.lock().await;
        for chunk in buf.chunks(16 * 1024) {
            let _ = self
                .iface
                .bulk_out(self.ep_out, chunk.to_vec())
                .await
                .into_result()
                .map_err(|e| AdbError::Usb(format!("bulk out {:#04x}: {e}", self.ep_out)))?;
        }
        // A full-sized packet needs a zero-length packet so the device sees the
        // end of the transfer. A failed ZLP is not fatal: macOS often rejects a
        // zero-length WritePipe, and the short header that follows still frames ADB.
        if !buf.is_empty() && self.mps_out > 0 && buf.len() % self.mps_out == 0 {
            if let Err(e) = self
                .iface
                .bulk_out(self.ep_out, Vec::new())
                .await
                .into_result()
            {
                log::debug!("zero-length packet on {:#04x}: {e}", self.ep_out);
            }
        }
        Ok(())
    }
}

fn round_up(n: usize, m: usize) -> usize {
    if m == 0 {
        return n;
    }
    (n + m - 1) / m * m
}
