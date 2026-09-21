use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

use super::protocol::AdbError;

#[async_trait]
pub trait AdbIo: Send + Sync {
    async fn read_exact(&self, buf: &mut [u8]) -> Result<(), AdbError>;
    async fn write_all(&self, buf: &[u8]) -> Result<(), AdbError>;
}

pub struct StreamIo<R, W> {
    reader: Mutex<R>,
    writer: Mutex<W>,
}

impl<R, W> StreamIo<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
        }
    }
}

#[async_trait]
impl<R, W> AdbIo for StreamIo<R, W>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    async fn read_exact(&self, buf: &mut [u8]) -> Result<(), AdbError> {
        let mut reader = self.reader.lock().await;
        reader
            .read_exact(buf)
            .await
            .map(|_| ())
            .map_err(|e| AdbError::Io(e.to_string()))
    }

    async fn write_all(&self, buf: &[u8]) -> Result<(), AdbError> {
        let mut writer = self.writer.lock().await;
        writer
            .write_all(buf)
            .await
            .map_err(|e| AdbError::Io(e.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|e| AdbError::Io(e.to_string()))
    }
}
