//! Native transport. The browser host does not use this crate: JS is the transport
//! and calls `droidmirror-client` directly.
//!
//! `NativeAdb` speaks the ADB protocol (the same framing, RSA auth, and stream mux as
//! `webadb-rs`) over `nusb` or TCP. The RSA authorization prompt is never skipped.

mod adb;

use async_trait::async_trait;

pub use adb::{adb_serials, AdbError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(pub u32);

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error(transparent)]
    Adb(#[from] AdbError),
}

pub type Result<T> = std::result::Result<T, TransportError>;

#[async_trait]
pub trait Transport {
    async fn open_stream(&mut self, dest: &str) -> Result<StreamId>;
    async fn read(&mut self, s: StreamId) -> Result<Vec<u8>>;
    async fn write(&mut self, s: StreamId, data: &[u8]) -> Result<()>;
    async fn close(&mut self, s: StreamId) -> Result<()>;
    async fn push_file(&mut self, data: &[u8], remote: &str) -> Result<()>;
    async fn shell(&mut self, cmd: &str) -> Result<String>;
}

pub struct NativeAdb {
    inner: Backend,
}

enum Backend {
    Device(adb::AdbClient),
    Server(adb::AdbServer),
}

async fn try_direct_usb(serial: Option<&str>) -> std::result::Result<NativeAdb, adb::AdbError> {
    let io = adb::open_usb(serial).await?;
    let key = adb::load_or_create_key()?;
    let inner = adb::AdbClient::connect(io, key).await?;
    Ok(NativeAdb {
        inner: Backend::Device(inner),
    })
}

fn transfer_failed(err: &adb::AdbError) -> bool {
    let text = err.to_string().to_ascii_lowercase();
    text.contains("unknown error") || text.contains("bulk in") || text.contains("bulk out")
}

impl NativeAdb {
    pub async fn connect_usb(serial: Option<&str>) -> Result<Self> {
        match try_direct_usb(serial).await {
            Ok(adb) => Ok(adb),
            Err(e) if adb::usb_is_busy(&e) || transfer_failed(&e) => {
                log::warn!(
                    "{e}; trying the adb server at 127.0.0.1:5037 (start it with `adb devices` if it is not running)"
                );
                match adb::AdbServer::connect("127.0.0.1:5037", serial).await {
                    Ok(server) => Ok(Self {
                        inner: Backend::Server(server),
                    }),
                    Err(server_err) => Err(adb::AdbError::Msg(format!(
                        "{e}. Could not use the adb server either ({server_err}). \
                         Run `adb devices` so the phone shows as `device`, or pass --serial."
                    ))
                    .into()),
                }
            }
            Err(e) => Err(e.into()),
        }
    }

    pub async fn connect_tcp(addr: &str) -> Result<Self> {
        let io = adb::open_tcp(addr).await?;
        let key = adb::load_or_create_key()?;
        let inner = adb::AdbClient::connect(io, key).await?;
        Ok(Self {
            inner: Backend::Device(inner),
        })
    }

    pub fn banner(&self) -> String {
        match &self.inner {
            Backend::Device(inner) => inner.banner().to_string(),
            Backend::Server(inner) => inner.banner(),
        }
    }

    /// Shared (`&self`) stream ops so a shell drain and the video reader can run together.
    pub async fn open(&self, dest: &str) -> Result<StreamId> {
        let id = match &self.inner {
            Backend::Device(inner) => inner.open_stream(dest).await?,
            Backend::Server(inner) => inner.open_stream(dest).await?,
        };
        Ok(StreamId(id))
    }

    pub async fn read_stream(&self, s: StreamId) -> Result<Vec<u8>> {
        Ok(match &self.inner {
            Backend::Device(inner) => inner.read(s.0).await?,
            Backend::Server(inner) => inner.read(s.0).await?,
        })
    }

    pub async fn write_stream(&self, s: StreamId, data: &[u8]) -> Result<()> {
        match &self.inner {
            Backend::Device(inner) => inner.write(s.0, data).await?,
            Backend::Server(inner) => inner.write(s.0, data).await?,
        }
        Ok(())
    }

    pub async fn close_stream(&self, s: StreamId) -> Result<()> {
        match &self.inner {
            Backend::Device(inner) => inner.close_stream(s.0).await?,
            Backend::Server(inner) => inner.close_stream(s.0).await?,
        }
        Ok(())
    }

    pub async fn shell_cmd(&self, cmd: &str) -> Result<String> {
        Ok(match &self.inner {
            Backend::Device(inner) => inner.shell(cmd).await?,
            Backend::Server(inner) => inner.shell(cmd).await?,
        })
    }

    pub async fn push(&self, data: &[u8], remote: &str) -> Result<()> {
        let mode = if remote.ends_with(".dex") {
            0o100644
        } else {
            0o100755
        };
        match &self.inner {
            Backend::Device(inner) => inner.push(data, remote, mode).await?,
            Backend::Server(inner) => inner.push(data, remote, mode).await?,
        }
        Ok(())
    }
}

#[async_trait]
impl Transport for NativeAdb {
    async fn open_stream(&mut self, dest: &str) -> Result<StreamId> {
        self.open(dest).await
    }

    async fn read(&mut self, s: StreamId) -> Result<Vec<u8>> {
        self.read_stream(s).await
    }

    async fn write(&mut self, s: StreamId, data: &[u8]) -> Result<()> {
        self.write_stream(s, data).await
    }

    async fn close(&mut self, s: StreamId) -> Result<()> {
        self.close_stream(s).await
    }

    async fn push_file(&mut self, data: &[u8], remote: &str) -> Result<()> {
        self.push(data, remote).await
    }

    async fn shell(&mut self, cmd: &str) -> Result<String> {
        self.shell_cmd(cmd).await
    }
}
