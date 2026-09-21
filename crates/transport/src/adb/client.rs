//! Multiplexed ADB connection. One reader task; writes interleave on a separate lock.
//!
//! This is the native counterpart of `webadb-rs`'s client: same CNXN / AUTH / OPEN /
//! WRTE / OKAY / CLSE sequence, with the USB backend swapped for `nusb` or TCP.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use super::auth::AdbKeyPair;
use super::io::AdbIo;
use super::protocol::{
    checksum, AdbError, Command, Packet, ADB_MAXDATA, ADB_VERSION, AUTH_RSAPUBLICKEY,
    AUTH_SIGNATURE, AUTH_TOKEN, CNXN_BANNER,
};

const MAX_PAYLOAD: u32 = 4 * 1024 * 1024;

struct StreamSlot {
    remote_id: AtomicU32,
    tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}

enum Ack {
    Okay { remote_id: u32 },
    Closed,
}

struct Shared {
    io: Arc<dyn AdbIo>,
    max_payload: AtomicU32,
    next_local: AtomicU32,
    streams: Mutex<HashMap<u32, Arc<StreamSlot>>>,
    waiters: Mutex<HashMap<u32, oneshot::Sender<Ack>>>,
}

pub struct AdbClient {
    shared: Arc<Shared>,
    reader: JoinHandle<()>,
    banner: String,
}

impl Drop for AdbClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl AdbClient {
    pub async fn connect(io: Arc<dyn AdbIo>, key: AdbKeyPair) -> Result<Self, AdbError> {
        let banner = handshake(&*io, &key).await?;
        let shared = Arc::new(Shared {
            io,
            max_payload: AtomicU32::new(banner.max_payload),
            next_local: AtomicU32::new(1),
            streams: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
        });
        let reader_shared = Arc::clone(&shared);
        let reader = tokio::spawn(async move {
            if let Err(e) = reader_loop(reader_shared).await {
                log::debug!("adb reader ended: {e}");
            }
        });
        Ok(Self {
            shared,
            reader,
            banner: banner.banner,
        })
    }

    pub fn banner(&self) -> &str {
        &self.banner
    }

    pub async fn open_stream(&self, dest: &str) -> Result<u32, AdbError> {
        let local = self.shared.next_local.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(128);
        let slot = Arc::new(StreamSlot {
            remote_id: AtomicU32::new(0),
            tx: Mutex::new(Some(tx)),
            rx: Mutex::new(rx),
        });
        self.shared.streams.lock().await.insert(local, Arc::clone(&slot));
        let (ack_tx, ack_rx) = oneshot::channel();
        self.shared.waiters.lock().await.insert(local, ack_tx);

        let mut payload = dest.as_bytes().to_vec();
        if !payload.ends_with(&[0]) {
            payload.push(0);
        }
        write_packet(&*self.shared.io, Command::Open, local, 0, &payload).await?;

        match timeout(Duration::from_secs(10), ack_rx).await {
            Ok(Ok(Ack::Okay { remote_id })) => {
                slot.remote_id.store(remote_id, Ordering::Relaxed);
                Ok(local)
            }
            Ok(Ok(Ack::Closed)) => {
                self.shared.streams.lock().await.remove(&local);
                Err(AdbError::Closed)
            }
            Ok(Err(_)) => Err(AdbError::Closed),
            Err(_) => {
                self.shared.waiters.lock().await.remove(&local);
                self.shared.streams.lock().await.remove(&local);
                Err(AdbError::Timeout)
            }
        }
    }

    pub async fn read(&self, local_id: u32) -> Result<Vec<u8>, AdbError> {
        let slot = self
            .shared
            .streams
            .lock()
            .await
            .get(&local_id)
            .cloned()
            .ok_or(AdbError::Closed)?;
        let mut rx = slot.rx.lock().await;
        rx.recv().await.ok_or(AdbError::Closed)
    }

    pub async fn write(&self, local_id: u32, data: &[u8]) -> Result<(), AdbError> {
        let slot = self
            .shared
            .streams
            .lock()
            .await
            .get(&local_id)
            .cloned()
            .ok_or(AdbError::Closed)?;
        let remote = slot.remote_id.load(Ordering::Relaxed);
        if remote == 0 {
            return Err(AdbError::Msg("stream not open".into()));
        }
        let max = self.shared.max_payload.load(Ordering::Relaxed).max(1) as usize;
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[]]
        } else {
            data.chunks(max).collect()
        };
        for chunk in chunks {
            let (ack_tx, ack_rx) = oneshot::channel();
            self.shared.waiters.lock().await.insert(local_id, ack_tx);
            write_packet(&*self.shared.io, Command::Wrte, local_id, remote, chunk).await?;
            match timeout(Duration::from_secs(15), ack_rx).await {
                Ok(Ok(Ack::Okay { .. })) => {}
                Ok(Ok(Ack::Closed)) => return Err(AdbError::Closed),
                Ok(Err(_)) => return Err(AdbError::Closed),
                Err(_) => return Err(AdbError::Timeout),
            }
        }
        Ok(())
    }

    pub async fn close_stream(&self, local_id: u32) -> Result<(), AdbError> {
        let slot = self.shared.streams.lock().await.remove(&local_id);
        let Some(slot) = slot else {
            return Ok(());
        };
        let remote = slot.remote_id.load(Ordering::Relaxed);
        let (ack_tx, ack_rx) = oneshot::channel();
        self.shared.waiters.lock().await.insert(local_id, ack_tx);
        let _ = write_packet(&*self.shared.io, Command::Clse, local_id, remote, &[]).await;
        let _ = timeout(Duration::from_secs(2), ack_rx).await;
        if let Some(tx) = slot.tx.lock().await.take() {
            drop(tx);
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

    /// SYNC `SEND`. `mode` is a full `st_mode` (e.g. `0o100755` = 33261).
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
                .map_err(|_| AdbError::Timeout)?
                .map_err(|_| AdbError::Closed)?;
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
}

struct Banner {
    banner: String,
    max_payload: u32,
}

async fn handshake(io: &dyn AdbIo, key: &AdbKeyPair) -> Result<Banner, AdbError> {
    write_packet(io, Command::Cnxn, ADB_VERSION, ADB_MAXDATA, CNXN_BANNER).await?;
    let pubkey = key.android_public_key("droidmirror@host")?;
    let mut sent_signature = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return Err(AdbError::Timeout);
        }
        let (pkt, data) = timeout(left, read_packet(io))
            .await
            .map_err(|_| AdbError::Timeout)??;
        match pkt.command {
            Command::Cnxn => {
                let banner = String::from_utf8_lossy(&data)
                    .trim_matches('\0')
                    .to_string();
                let max_payload = pkt.arg1.clamp(4096, ADB_MAXDATA);
                return Ok(Banner {
                    banner,
                    max_payload,
                });
            }
            Command::Auth if pkt.arg0 == AUTH_TOKEN && !sent_signature => {
                let sig = key.sign_token(&data)?;
                write_packet(io, Command::Auth, AUTH_SIGNATURE, 0, &sig).await?;
                sent_signature = true;
            }
            Command::Auth if pkt.arg0 == AUTH_TOKEN => {
                log::info!("accept the USB debugging RSA prompt on the device");
                write_packet(io, Command::Auth, AUTH_RSAPUBLICKEY, 0, &pubkey).await?;
            }
            other => {
                return Err(AdbError::Protocol(format!(
                    "unexpected {other:?} during connect"
                )));
            }
        }
    }
}

async fn reader_loop(shared: Arc<Shared>) -> Result<(), AdbError> {
    loop {
        let (pkt, data) = read_packet(&*shared.io).await?;
        match pkt.command {
            Command::Okay => {
                let local = pkt.arg1;
                if let Some(slot) = shared.streams.lock().await.get(&local) {
                    if slot.remote_id.load(Ordering::Relaxed) == 0 {
                        slot.remote_id.store(pkt.arg0, Ordering::Relaxed);
                    }
                }
                if let Some(w) = shared.waiters.lock().await.remove(&local) {
                    let _ = w.send(Ack::Okay {
                        remote_id: pkt.arg0,
                    });
                }
            }
            Command::Wrte => {
                let local = pkt.arg1;
                let remote = pkt.arg0;
                write_packet(&*shared.io, Command::Okay, local, remote, &[]).await?;
                if let Some(slot) = shared.streams.lock().await.get(&local).cloned() {
                    let tx = slot.tx.lock().await;
                    if let Some(tx) = tx.as_ref() {
                        if tx.try_send(data).is_err() {
                            log::warn!("adb stream {local} backed up; dropped a payload");
                        }
                    }
                }
            }
            Command::Clse => {
                let local = pkt.arg1;
                if let Some(w) = shared.waiters.lock().await.remove(&local) {
                    let _ = w.send(Ack::Closed);
                }
                if let Some(slot) = shared.streams.lock().await.get(&local) {
                    if let Some(tx) = slot.tx.lock().await.take() {
                        drop(tx);
                    }
                }
            }
            Command::Auth | Command::Cnxn | Command::Sync | Command::Open => {
                log::debug!("adb ignoring {:?} after connect", pkt.command);
            }
        }
    }
}

async fn write_packet(
    io: &dyn AdbIo,
    command: Command,
    arg0: u32,
    arg1: u32,
    data: &[u8],
) -> Result<(), AdbError> {
    let pkt = Packet::new(command, arg0, arg1, data);
    // adbd reads the 24-byte header and the payload as two USB transfers.
    // A single combined write is one short packet; the device rejects it and
    // macOS completes the following read with kIOReturnIsoTooNew (0xe00002ed),
    // which nusb reports as "unknown error".
    io.write_all(&pkt.to_bytes()).await?;
    if !data.is_empty() {
        io.write_all(data).await?;
    }
    Ok(())
}

async fn read_packet(io: &dyn AdbIo) -> Result<(Packet, Vec<u8>), AdbError> {
    let mut hdr = [0u8; 24];
    io.read_exact(&mut hdr).await?;
    let pkt = Packet::from_bytes(&hdr)?;
    if pkt.data_length > MAX_PAYLOAD {
        return Err(AdbError::Protocol(format!(
            "payload {} exceeds cap",
            pkt.data_length
        )));
    }
    let mut data = vec![0u8; pkt.data_length as usize];
    if pkt.data_length > 0 {
        io.read_exact(&mut data).await?;
        if pkt.data_crc32 != 0 && pkt.data_crc32 != checksum(&data) {
            return Err(AdbError::Protocol("checksum mismatch".into()));
        }
    }
    Ok((pkt, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::io::StreamIo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_pkt<R: AsyncReadExt + Unpin>(r: &mut R) -> (Command, u32, u32, Vec<u8>) {
        let mut hdr = [0u8; 24];
        r.read_exact(&mut hdr).await.unwrap();
        let pkt = Packet::from_bytes(&hdr).unwrap();
        let mut data = vec![0u8; pkt.data_length as usize];
        if pkt.data_length > 0 {
            r.read_exact(&mut data).await.unwrap();
        }
        (pkt.command, pkt.arg0, pkt.arg1, data)
    }

    async fn write_pkt<W: AsyncWriteExt + Unpin>(
        w: &mut W,
        command: Command,
        arg0: u32,
        arg1: u32,
        data: &[u8],
    ) {
        let pkt = Packet::new(command, arg0, arg1, data);
        w.write_all(&pkt.to_bytes()).await.unwrap();
        w.write_all(data).await.unwrap();
    }

    #[tokio::test]
    async fn connect_open_write_read_against_fake_adbd() {
        let (client_stream, mut server) = tokio::io::duplex(64 * 1024);
        let (cr, cw) = tokio::io::split(client_stream);
        let io: Arc<dyn AdbIo> = Arc::new(StreamIo::new(cr, cw));
        let key = AdbKeyPair::generate().unwrap();

        let server_task = tokio::spawn(async move {
            let (cmd, _, _, _) = read_pkt(&mut server).await;
            assert_eq!(cmd, Command::Cnxn);
            let token = b"0123456789abcdef0123";
            write_pkt(&mut server, Command::Auth, AUTH_TOKEN, 0, token).await;
            let (cmd, arg0, _, sig) = read_pkt(&mut server).await;
            assert_eq!(cmd, Command::Auth);
            assert_eq!(arg0, AUTH_SIGNATURE);
            assert_eq!(sig.len(), 256);
            write_pkt(
                &mut server,
                Command::Cnxn,
                ADB_VERSION,
                64 * 1024,
                b"device::fake\0",
            )
            .await;

            let (cmd, local, _, dest) = read_pkt(&mut server).await;
            assert_eq!(cmd, Command::Open);
            assert!(dest.starts_with(b"localabstract:droidmirror"));
            let remote = 9u32;
            write_pkt(&mut server, Command::Okay, remote, local, &[]).await;

            let (cmd, _, _, data) = read_pkt(&mut server).await;
            assert_eq!(cmd, Command::Wrte);
            assert_eq!(data, b"ping");
            write_pkt(&mut server, Command::Okay, remote, local, &[]).await;
            write_pkt(&mut server, Command::Wrte, remote, local, b"hello").await;
            let (cmd, _, _, _) = read_pkt(&mut server).await;
            assert_eq!(cmd, Command::Okay);
        });

        let client = AdbClient::connect(io, key).await.unwrap();
        assert!(client.banner().contains("fake"));
        let id = client.open_stream("localabstract:droidmirror").await.unwrap();
        client.write(id, b"ping").await.unwrap();
        let got = client.read(id).await.unwrap();
        assert_eq!(got, b"hello");
        server_task.await.unwrap();
    }
}
