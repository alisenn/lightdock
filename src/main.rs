//! lightdock: starts a Docker engine inside WSL2 on demand, proxies the Docker
//! API to it, and shuts the whole WSL VM down when nothing is running.
//!
//! Windows `docker` CLI  ->  127.0.0.1:2375 (lightdock)  ->  dockerd in WSL (:2376)

use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{copy_bidirectional, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

const LISTEN_ADDR: &str = "127.0.0.1:2375";
const ENGINE_ADDR: &str = "127.0.0.1:2376";

struct Config {
    distro: String,
    idle_timeout: Duration,
}

impl Config {
    fn from_env() -> Self {
        let distro = std::env::var("LIGHTDOCK_DISTRO").unwrap_or_else(|_| "Ubuntu".into());
        let mins: u64 = std::env::var("LIGHTDOCK_IDLE_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        Self { distro, idle_timeout: Duration::from_secs(mins * 60) }
    }
}

struct Shared {
    cfg: Config,
    engine: Mutex<Option<Child>>,
    active: AtomicUsize,
    last_activity: Mutex<Instant>,
}

fn wsl() -> Command {
    let mut cmd = Command::new("wsl.exe");
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

async fn engine_ready() -> bool {
    match http_get(ENGINE_ADDR, "/_ping").await {
        Some(resp) => resp.starts_with("HTTP/1.1 200") || resp.starts_with("HTTP/1.0 200"),
        None => false,
    }
}

async fn http_get(addr: &str, path: &str) -> Option<String> {
    let work = async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        let req = format!("GET {path} HTTP/1.0\r\nHost: lightdock\r\n\r\n");
        s.write_all(req.as_bytes()).await.ok()?;
        let mut buf = String::new();
        s.read_to_string(&mut buf).await.ok()?;
        Some(buf)
    };
    tokio::time::timeout(Duration::from_secs(3), work).await.ok().flatten()
}

async fn has_running_containers() -> bool {
    match http_get(ENGINE_ADDR, "/containers/json").await {
        // Body of an empty list is "[]". If the query fails, be safe and say "yes".
        Some(resp) => !resp.trim_end().ends_with("[]"),
        None => true,
    }
}

impl Shared {
    /// Starts WSL + dockerd if it is not already up. Safe to call concurrently.
    async fn ensure_engine(&self) -> Result<(), String> {
        let mut guard = self.engine.lock().await;
        if guard.is_some() && engine_ready().await {
            return Ok(());
        }
        if let Some(mut old) = guard.take() {
            let _ = old.kill().await;
        }
        eprintln!("[lightdock] starting engine in WSL distro '{}'...", self.cfg.distro);

        // The wsl.exe process stays alive for as long as dockerd runs, which also
        // keeps the WSL VM from shutting itself down underneath us.
        let child = wsl()
            .args(["-d", &self.cfg.distro, "-u", "root", "--", "dockerd"])
            .args(["-H", "tcp://0.0.0.0:2376", "-H", "unix:///var/run/docker.sock"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run wsl.exe: {e}"))?;
        *guard = Some(child);

        for _ in 0..60 {
            if engine_ready().await {
                eprintln!("[lightdock] engine is ready");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err("engine did not become ready within 30s (is docker installed? run `lightdock setup`)".into())
    }

    async fn stop_engine(&self) {
        let mut guard = self.engine.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
        let _ = wsl().args(["--terminate", &self.cfg.distro]).status().await;
        eprintln!("[lightdock] engine stopped, WSL memory released");
    }

    async fn touch(&self) {
        *self.last_activity.lock().await = Instant::now();
    }
}

async fn handle_client(shared: Arc<Shared>, mut client: TcpStream) {
    shared.active.fetch_add(1, Ordering::SeqCst);
    shared.touch().await;

    let result = async {
        shared.ensure_engine().await?;
        let mut engine = TcpStream::connect(ENGINE_ADDR).await.map_err(|e| e.to_string())?;
        copy_bidirectional(&mut client, &mut engine).await.map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }
    .await;

    if let Err(e) = result {
        eprintln!("[lightdock] connection error: {e}");
    }
    shared.touch().await;
    shared.active.fetch_sub(1, Ordering::SeqCst);
}

async fn idle_watcher(shared: Arc<Shared>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if shared.engine.lock().await.is_none() || shared.active.load(Ordering::SeqCst) > 0 {
            continue;
        }
        if shared.last_activity.lock().await.elapsed() < shared.cfg.idle_timeout {
            continue;
        }
        if has_running_containers().await {
            shared.touch().await; // containers are working, check again later
            continue;
        }
        shared.stop_engine().await;
    }
}

async fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let shared = Arc::new(Shared {
        cfg: Config::from_env(),
        engine: Mutex::new(None),
        active: AtomicUsize::new(0),
        last_activity: Mutex::new(Instant::now()),
    });

    let listener = TcpListener::bind(LISTEN_ADDR).await?;
    eprintln!("[lightdock] listening on {LISTEN_ADDR}  (set DOCKER_HOST=tcp://{LISTEN_ADDR})");
    tokio::spawn(idle_watcher(shared.clone()));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (client, _) = accepted?;
                tokio::spawn(handle_client(shared.clone(), client));
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("[lightdock] shutting down");
                shared.stop_engine().await;
                return Ok(());
            }
        }
    }
}

/// Installs docker inside the WSL distro (Ubuntu/Debian).
async fn setup() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = Config::from_env();
    println!("Installing Docker Engine inside WSL distro '{}'...", cfg.distro);
    let status = wsl()
        .args(["-d", &cfg.distro, "-u", "root", "--", "sh", "-c", "command -v dockerd >/dev/null || (curl -fsSL https://get.docker.com | sh)"])
        .status()
        .await?;
    if !status.success() {
        return Err("setup failed".into());
    }
    println!("\nDone. Now run `lightdock` and in another terminal:");
    println!("  setx DOCKER_HOST tcp://{LISTEN_ADDR}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        None | Some("run") => run_daemon().await,
        Some("setup") => setup().await,
        Some(_) => {
            println!("usage: lightdock [run|setup]\n\nenv: LIGHTDOCK_DISTRO (default Ubuntu), LIGHTDOCK_IDLE_MINUTES (default 5)");
            Ok(())
        }
    }
}
