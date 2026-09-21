//! Talk to a device through the local `adb` server (`127.0.0.1:5037`).
//!
//! Used when USB exclusive access fails because `adb` already owns the interface.
//! Each service (`shell:`, `sync:`, `localabstract:`) is its own TCP connection:
//! `host:transport:<serial>`, then the service name. Bytes after `OKAY` are the
//! raw service stream, not CNXN packets.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::protocol::AdbError;

struct Stream {
    write: Mutex<OwnedWriteHalf>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    reader: JoinHandle<()>,
}

pub struct AdbServer {
    addr: String,
    serial: String,
    next_id: AtomicU32,
    streams: Mutex<HashMap<u32, Arc<Stream>>>,
}

impl Drop for AdbServer {
    fn drop(&mut self) {
        if let Ok(streams) = self.streams.try_lock() {
            for stream in streams.values() {
                stream.reader.abort();
            }
        }
    }
}

impl AdbServer {
    pub async fn connect(addr: &str, serial: Option<&str>) -> Result<Self, AdbError> {
        let mut sock = TcpStream::connect(addr)
            .await
            .map_err(|e| AdbError::Io(format!("adb server {addr}: {e}")))?;
        sock.set_nodelay(true).ok();
        write_request(&mut sock, "host:devices").await?;
        expect_okay(&mut sock).await?;
        let list = read_hex_payload(&mut sock).await?;
        let text = String::from_utf8_lossy(&list);
        let serial = pick_serial(&text, serial)?;
        drop(sock);
        log::info!("adb server {addr} transport {serial}");
        Ok(Self {
            addr: addr.to_string(),
            serial,
            next_id: AtomicU32::new(1),
            streams: Mutex::new(HashMap::new()),
        })
    }

    pub fn banner(&self) -> String {
        format!("adb-server {}", self.serial)
    }

    pub async fn open_stream(&self, dest: &str) -> Result<u32, AdbError> {
        let service = dest.trim_end_matches('\0');
        let mut sock = self.dial().await?;
        write_request(&mut sock, &format!("host:transport:{}", self.serial)).await?;
        expect_okay(&mut sock).await?;
        write_request(&mut sock, service).await?;
        expect_okay(&mut sock).await?;

        let (read, write) = sock.into_split();
        let (tx, rx) = mpsc::channel(128);
        let reader = tokio::spawn(async move {
            pump(read, tx).await;
        });
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.streams.lock().await.insert(
            id,
            Arc::new(Stream {
                write: Mutex::new(write),
                rx: Mutex::new(rx),
                reader,
            }),
        );
        Ok(id)
    }

    pub async fn read(&self, id: u32) -> Result<Vec<u8>, AdbError> {
        let stream = self.stream(id).await?;
        let mut rx = stream.rx.lock().await;
        rx.recv().await.ok_or(AdbError::Closed)
    }

    pub async fn write(&self, id: u32, data: &[u8]) -> Result<(), AdbError> {
        if data.is_empty() {
            return Ok(());
        }
        let stream = self.stream(id).await?;
        let mut write = stream.write.lock().await;
        write
            .write_all(data)
            .await
            .map_err(|e| AdbError::Io(e.to_string()))
    }

    pub async fn close_stream(&self, id: u32) -> Result<(), AdbError> {
        if let Some(stream) = self.streams.lock().await.remove(&id) {
            stream.reader.abort();
        }
        Ok(())
    }

    pub async fn shell(&self, cmd: &str) -> Result<String, AdbError> {
        let id = self.open_stream(&format!("shell:{cmd}")).await?;
        let mut out = Vec::new();
        loop {
            match timeout(Duration::from_secs(60), self.read(id)).await {
                Ok(Ok(chunk)) => out.extend_from_slice(&chunk),
                _ => break,
            }
        }
        let _ = self.close_stream(id).await;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    pub async fn push(&self, data: &[u8], remote: &str, mode: u32) -> Result<(), AdbError> {
        let id = self.open_stream("sync:").await?;
        let spec = format!("{remote},{mode}");
        let mut send = Vec::from(&b"SEND"[..]);
        send.extend_from_slice(&(spec.len() as u32).to_le_bytes());
        send.extend_from_slice(spec.as_bytes());
        self.write(id, &send).await?;
        for chunk in data.chunks(64 * 1024) {
            let mut pkt = Vec::from(&b"DATA"[..]);
            pkt.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
            pkt.extend_from_slice(chunk);
            self.write(id, &pkt).await?;
        }
        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);
        let mut done = Vec::from(&b"DONE"[..]);
        done.extend_from_slice(&mtime.to_le_bytes());
        self.write(id, &done).await?;

        let mut acc = Vec::new();
        let status = loop {
            let chunk = timeout(Duration::from_secs(20), self.read(id))
                .await
                .map_err(|_| AdbError::Timeout)??;
            acc.extend_from_slice(&chunk);
            if acc.len() >= 8 {
                break acc;
            }
        };
        let _ = self.write(id, b"QUIT\0\0\0\0").await;
        let _ = self.close_stream(id).await;
        if status.starts_with(b"OKAY") {
            Ok(())
        } else if status.starts_with(b"FAIL") {
            let n = u32::from_le_bytes(status[4..8].try_into().unwrap()) as usize;
            let msg = if status.len() >= 8 + n {
                String::from_utf8_lossy(&status[8..8 + n]).into_owned()
            } else {
                "push failed".into()
            };
            Err(AdbError::Msg(msg))
        } else {
            Err(AdbError::Protocol("sync reply was not OKAY/FAIL".into()))
        }
    }

    async fn dial(&self) -> Result<TcpStream, AdbError> {
        let sock = TcpStream::connect(&self.addr)
            .await
            .map_err(|e| AdbError::Io(format!("adb server {}: {e}", self.addr)))?;
        sock.set_nodelay(true).ok();
        Ok(sock)
    }

    async fn stream(&self, id: u32) -> Result<Arc<Stream>, AdbError> {
        self.streams
            .lock()
            .await
            .get(&id)
            .cloned()
            .ok_or(AdbError::Closed)
    }
}

fn pick_serial(list: &str, want: Option<&str>) -> Result<String, AdbError> {
    let mut online = Vec::new();
    let mut other = Vec::new();
    for line in list.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let serial = parts.next().unwrap_or("").trim();
        let state = parts.next().unwrap_or("").trim();
        if serial.is_empty() {
            continue;
        }
        if state == "device" {
            online.push(serial.to_string());
        } else {
            other.push(format!("{serial} ({state})"));
        }
    }
    if let Some(want) = want {
        if online.iter().any(|s| s == want) {
            return Ok(want.to_string());
        }
        if other.iter().any(|s| s.starts_with(want)) {
            return Err(AdbError::Msg(format!(
                "{want} is not authorized. Accept the USB debugging prompt, then retry"
            )));
        }
        return Err(AdbError::Msg(format!(
            "adb server has no device {want} (online: {})",
            if online.is_empty() {
                "none".into()
            } else {
                online.join(", ")
            }
        )));
    }
    let hardware: Vec<String> = online
        .iter()
        .filter(|s| !s.starts_with("emulator-"))
        .cloned()
        .collect();
    let candidates = if hardware.is_empty() {
        online
    } else {
        hardware
    };
    match candidates.len() {
        1 => Ok(candidates.into_iter().next().unwrap()),
        0 => Err(AdbError::Msg(format!(
            "adb server has no online device{}",
            if other.is_empty() {
                String::new()
            } else {
                format!(" ({})", other.join(", "))
            }
        ))),
        _ => Err(AdbError::Msg(format!(
            "multiple devices ({}); pass --serial",
            candidates.join(", ")
        ))),
    }
}

async fn pump(mut read: OwnedReadHalf, tx: mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match read.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        }
    }
}

async fn write_request(sock: &mut TcpStream, msg: &str) -> Result<(), AdbError> {
    let hdr = format!("{:04x}", msg.len());
    sock.write_all(hdr.as_bytes())
        .await
        .map_err(|e| AdbError::Io(e.to_string()))?;
    sock.write_all(msg.as_bytes())
        .await
        .map_err(|e| AdbError::Io(e.to_string()))?;
    Ok(())
}

async fn expect_okay(sock: &mut TcpStream) -> Result<(), AdbError> {
    let mut status = [0u8; 4];
    sock.read_exact(&mut status)
        .await
        .map_err(|e| AdbError::Io(e.to_string()))?;
    match &status {
        b"OKAY" => Ok(()),
        b"FAIL" => {
            let msg = read_hex_payload(sock).await?;
            Err(AdbError::Msg(
                String::from_utf8_lossy(&msg).trim().to_string(),
            ))
        }
        other => Err(AdbError::Protocol(format!(
            "adb server status {}",
            String::from_utf8_lossy(other)
        ))),
    }
}

async fn read_hex_payload(sock: &mut TcpStream) -> Result<Vec<u8>, AdbError> {
    let mut len_buf = [0u8; 4];
    sock.read_exact(&mut len_buf)
        .await
        .map_err(|e| AdbError::Io(e.to_string()))?;
    let len_str = std::str::from_utf8(&len_buf)
        .map_err(|_| AdbError::Protocol("adb length is not hex".into()))?;
    let len = usize::from_str_radix(len_str, 16)
        .map_err(|_| AdbError::Protocol(format!("adb length {len_str}")))?;
    let mut body = vec![0u8; len];
    if len > 0 {
        sock.read_exact(&mut body)
            .await
            .map_err(|e| AdbError::Io(e.to_string()))?;
    }
    Ok(body)
}

pub fn usb_is_busy(err: &AdbError) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("exclusive")
        || text.contains("resource busy")
        || text.contains("already open")
        || text.contains("access denied")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_only_online_device() {
        let serial = pick_serial("ABC\tdevice\nDEF\tunauthorized\n", None).unwrap();
        assert_eq!(serial, "ABC");
    }

    #[test]
    fn prefers_a_phone_over_an_emulator() {
        let serial = pick_serial("emulator-5554\tdevice\n94LBA009AC\tdevice\n", None).unwrap();
        assert_eq!(serial, "94LBA009AC");
    }

    #[test]
    fn still_requires_serial_for_two_phones() {
        let err = pick_serial("AAA\tdevice\nBBB\tdevice\n", None).unwrap_err();
        assert!(err.to_string().contains("multiple devices"));
    }

    #[tokio::test]
    async fn shell_through_fake_adb_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    loop {
                        let req = match read_client_request(&mut sock).await {
                            Some(r) => r,
                            None => return,
                        };
                        if req == "host:devices" {
                            let _ = sock.write_all(b"OKAY").await;
                            let body = b"94LBA009AC\tdevice\n";
                            let hdr = format!("{:04x}", body.len());
                            let _ = sock.write_all(hdr.as_bytes()).await;
                            let _ = sock.write_all(body).await;
                            return;
                        } else if req.starts_with("host:transport:") {
                            let _ = sock.write_all(b"OKAY").await;
                        } else if req == "shell:echo hi" {
                            let _ = sock.write_all(b"OKAY").await;
                            let _ = sock.write_all(b"hi\n").await;
                            return;
                        } else {
                            let _ = sock.write_all(b"FAIL").await;
                            let msg = b"nope";
                            let hdr = format!("{:04x}", msg.len());
                            let _ = sock.write_all(hdr.as_bytes()).await;
                            let _ = sock.write_all(msg).await;
                            return;
                        }
                    }
                });
            }
        });

        let adb = AdbServer::connect(&format!("127.0.0.1:{port}"), None)
            .await
            .unwrap();
        assert_eq!(adb.serial, "94LBA009AC");
        let out = adb.shell("echo hi").await.unwrap();
        assert_eq!(out, "hi\n");
    }

    async fn read_client_request(sock: &mut TcpStream) -> Option<String> {
        let mut hdr = [0u8; 4];
        sock.read_exact(&mut hdr).await.ok()?;
        let n = usize::from_str_radix(std::str::from_utf8(&hdr).ok()?, 16).ok()?;
        let mut body = vec![0u8; n];
        sock.read_exact(&mut body).await.ok()?;
        String::from_utf8(body).ok()
    }
}
