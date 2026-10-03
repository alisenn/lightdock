//! Minimal Docker Engine API client (plain HTTP/1.0 over TCP, no dependencies).

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::ENGINE_ADDR;

/// Sends one request to the engine. HTTP/1.0 keeps responses un-chunked.
pub async fn request(method: &str, path: &str, timeout: Duration) -> Option<(u16, Vec<u8>)> {
    let work = async {
        let mut s = TcpStream::connect(ENGINE_ADDR).await.ok()?;
        let req = format!("{method} {path} HTTP/1.0\r\nHost: lightdock\r\nContent-Length: 0\r\n\r\n");
        s.write_all(req.as_bytes()).await.ok()?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.ok()?;
        parse_response(&buf)
    };
    tokio::time::timeout(timeout, work).await.ok().flatten()
}

fn parse_response(buf: &[u8]) -> Option<(u16, Vec<u8>)> {
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..split]);
    let status = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, buf[split + 4..].to_vec()))
}

pub async fn engine_ready() -> bool {
    matches!(request("GET", "/_ping", Duration::from_secs(3)).await, Some((200, _)))
}

pub async fn has_running_containers() -> bool {
    match request("GET", "/containers/json", Duration::from_secs(3)).await {
        Some((200, body)) => String::from_utf8_lossy(&body).trim() != "[]",
        // If the query fails, assume busy so we never kill a working engine.
        _ => true,
    }
}

/// Docker multiplexes stdout/stderr of non-TTY containers into 8-byte-headed
/// frames. Strip the headers; pass TTY (raw) output through unchanged.
pub fn demux_logs(raw: &[u8]) -> String {
    let looks_multiplexed = raw.len() >= 8 && raw[0] <= 2 && raw[1..4] == [0, 0, 0];
    if !looks_multiplexed {
        return String::from_utf8_lossy(raw).into_owned();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= raw.len() {
        let len = u32::from_be_bytes([raw[i + 4], raw[i + 5], raw[i + 6], raw[i + 7]]) as usize;
        let end = (i + 8 + len).min(raw.len());
        out.extend_from_slice(&raw[i + 8..end]);
        i = end;
    }
    String::from_utf8_lossy(&out).into_owned()
}
