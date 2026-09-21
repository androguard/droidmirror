use std::sync::Arc;

use tokio::net::TcpStream;

use super::io::{AdbIo, StreamIo};
use super::protocol::AdbError;

pub async fn open_tcp(addr: &str) -> Result<Arc<dyn AdbIo>, AdbError> {
    let stream = TcpStream::connect(addr)
        .await
        .map_err(|e| AdbError::Io(format!("tcp {addr}: {e}")))?;
    stream
        .set_nodelay(true)
        .map_err(|e| AdbError::Io(e.to_string()))?;
    let (r, w) = stream.into_split();
    let io: Arc<dyn AdbIo> = Arc::new(StreamIo::new(r, w));
    Ok(io)
}
