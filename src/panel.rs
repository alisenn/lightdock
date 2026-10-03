//! Tiny local web panel (127.0.0.1 only). Served on demand, so it costs no RAM
//! while closed. Every API call needs the per-run token in `X-Token`; a custom
//! header forces a CORS preflight that we never answer, so other websites
//! cannot drive the API, and the Host check blocks DNS rebinding.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::docker;
use crate::engine::Shared;
use crate::PANEL_ADDR;

const INDEX_HTML: &str = include_str!("panel.html");
const API_TIMEOUT: Duration = Duration::from_secs(35);

pub fn random_token() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("OS random source unavailable");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn serve(shared: Arc<Shared>, token: String) -> std::io::Result<()> {
    let listener = TcpListener::bind(PANEL_ADDR).await?;
    let token = Arc::new(token);
    loop {
        let (conn, _) = listener.accept().await?;
        let (shared, token) = (shared.clone(), token.clone());
        tokio::spawn(async move {
            let _ = handle(conn, shared, token).await;
        });
    }
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Reply {
    fn json(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self { status, content_type: "application/json", body: body.into() }
    }
    fn text(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self { status, content_type: "text/plain; charset=utf-8", body: body.into() }
    }
    fn error(status: u16, msg: &str) -> Self {
        Self::json(status, format!("{{\"error\":\"{msg}\"}}"))
    }
}

async fn handle(mut conn: TcpStream, shared: Arc<Shared>, token: Arc<String>) -> std::io::Result<()> {
    let mut buf = vec![0u8; 8192];
    let mut n = 0;
    loop {
        let r = conn.read(&mut buf[n..]).await?;
        if r == 0 {
            return Ok(());
        }
        n += r;
        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if n == buf.len() {
            return Ok(());
        }
    }
    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
    let mut lines = text.lines();
    let mut first = lines.next().unwrap_or("").split_whitespace();
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let header = |name: &str| {
        lines
            .clone()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)))
            .map(|(_, v)| v.trim().to_string())
    };

    let host_ok = matches!(header("host").as_deref(), Some("127.0.0.1:2377") | Some("localhost:2377"));
    let reply = if !host_ok {
        Reply::error(403, "bad host")
    } else {
        let path = target.split('?').next().unwrap_or("");
        if method == "GET" && path == "/" {
            Reply { status: 200, content_type: "text/html; charset=utf-8", body: INDEX_HTML.into() }
        } else if path.starts_with("/api/") {
            if header("x-token").as_deref() != Some(token.as_str()) {
                Reply::error(401, "bad token")
            } else {
                route(method, path, &shared).await
            }
        } else {
            Reply::error(404, "not found")
        }
    };

    let reason = match reply.status {
        200 => "OK",
        204 => "No Content",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Security-Policy: default-src 'self'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; frame-ancestors 'none'\r\nConnection: close\r\n\r\n",
        reply.status, reason, reply.content_type, reply.body.len()
    );
    conn.write_all(head.as_bytes()).await?;
    conn.write_all(&reply.body).await?;
    conn.shutdown().await
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || "_.-:".contains(c))
}

async fn route(method: &str, path: &str, shared: &Arc<Shared>) -> Reply {
    let parts: Vec<&str> = path.trim_start_matches("/api/").split('/').collect();

    match (method, parts.as_slice()) {
        ("GET", ["state"]) => {
            let body = format!(
                "{{\"running\":{},\"clients\":{},\"idle_seconds\":{},\"idle_timeout\":{},\"distro\":\"{}\"}}",
                shared.is_running(),
                shared.active_connections(),
                shared.idle_seconds().await,
                shared.cfg.idle_timeout.as_secs(),
                shared.cfg.distro.replace(['"', '\\'], "")
            );
            return Reply::json(200, body);
        }
        ("POST", ["engine", "start"]) => {
            return match shared.ensure_engine().await {
                Ok(()) => Reply::json(200, "{}"),
                Err(e) => Reply::error(409, &e.replace(['"', '\\'], "'")),
            };
        }
        ("POST", ["engine", "stop"]) => {
            shared.stop_engine().await;
            return Reply::json(200, "{}");
        }
        _ => {}
    }

    // Everything below talks to the engine; never wake it just for the panel.
    if !shared.is_running() {
        return Reply::error(409, "engine stopped");
    }

    let (docker_method, docker_path, wants_logs) = match (method, parts.as_slice()) {
        ("GET", ["containers"]) => ("GET", "/containers/json?all=1".to_string(), false),
        ("GET", ["images"]) => ("GET", "/images/json".to_string(), false),
        ("GET", ["containers", id, "logs"]) if valid_id(id) => {
            ("GET", format!("/containers/{id}/logs?stdout=1&stderr=1&tail=300"), true)
        }
        ("POST", ["containers", id, action @ ("start" | "stop" | "restart")]) if valid_id(id) => {
            shared.touch().await;
            ("POST", format!("/containers/{id}/{action}"), false)
        }
        ("DELETE", ["containers", id]) if valid_id(id) => {
            shared.touch().await;
            ("DELETE", format!("/containers/{id}?force=true"), false)
        }
        ("DELETE", ["images", id]) if valid_id(id) => {
            shared.touch().await;
            ("DELETE", format!("/images/{id}"), false)
        }
        _ => return Reply::error(404, "not found"),
    };

    match docker::request(docker_method, &docker_path, API_TIMEOUT).await {
        Some((status, body)) if wants_logs => Reply::text(status, docker::demux_logs(&body)),
        Some((status, body)) => Reply::json(status, body),
        None => Reply::error(409, "engine not responding"),
    }
}
