// Adapted for Timon. Not derived from Prodex source.
//! Talking to the recorder socket.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::recorder::event::MAX_REQUEST_BYTES;
use crate::recorder::protocol::{Request, Response};

/// Sends one request and waits for its response.
pub async fn send(socket: &Path, request: &Request) -> Result<Response> {
    send_line(socket, &serde_json::to_string(request)?).await
}

/// Sends an already-encoded request.
///
/// Used so the CLI can forward a producer's event exactly as written instead of
/// decoding and re-encoding it. A round trip through our own types would drop
/// the fields this build ignores, and then the daemon could not report that it
/// had ignored them.
pub async fn send_line(socket: &Path, request: &str) -> Result<Response> {
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let (read_half, mut write_half) = stream.into_split();

    let mut line = request.to_string();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await?;

    // Bounded so a misbehaving or compromised daemon cannot grow the client's
    // memory without limit.
    let mut reader = BufReader::new(read_half.take(MAX_REQUEST_BYTES as u64));
    let mut response = String::new();
    let read = reader.read_line(&mut response).await?;
    if read == 0 {
        return Err(anyhow!("recorder closed the connection without replying"));
    }
    serde_json::from_str(&response)
        .with_context(|| format!("decoding recorder reply: {}", response.trim()))
}
