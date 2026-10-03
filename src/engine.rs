//! Engine lifecycle (WSL2 + dockerd), the Docker API proxy and idle shutdown.

use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::{watch, Mutex};

use crate::docker;
use crate::{ENGINE_ADDR, PROXY_ADDR};

pub struct Config {
    pub distro: String,
    pub idle_timeout: Duration,
}

impl Config {
    pub fn from_env() -> Self {
        let distro = std::env::var("LIGHTDOCK_DISTRO").unwrap_or_else(|_| "Ubuntu".into());
        let mins: u64 = std::env::var("LIGHTDOCK_IDLE_MINUTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        Self { distro, idle_timeout: Duration::from_secs(mins * 60) }
    }
}

pub struct Shared {
    pub cfg: Config,
    engine: Mutex<Option<Child>>,
    active: AtomicUsize,
    last_activity: Mutex<Instant>,
    state: watch::Sender<bool>,
}

pub fn wsl() -> Command {
    let mut cmd = Command::new("wsl.exe");
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

impl Shared {
    pub fn new(cfg: Config) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            engine: Mutex::new(None),
            active: AtomicUsize::new(0),
            last_activity: Mutex::new(Instant::now()),
            state: watch::channel(false).0,
        })
    }

    /// Receiver that fires whenever the engine goes up or down.
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.state.subscribe()
    }

    pub fn is_running(&self) -> bool {
        *self.state.borrow()
    }

    pub fn active_connections(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    pub async fn idle_seconds(&self) -> u64 {
        self.last_activity.lock().await.elapsed().as_secs()
    }

    pub async fn touch(&self) {
        *self.last_activity.lock().await = Instant::now();
    }

    /// Starts WSL + dockerd if it is not already up. Safe to call concurrently.
    pub async fn ensure_engine(&self) -> Result<(), String> {
        let mut guard = self.engine.lock().await;
        if guard.is_some() && docker::engine_ready().await {
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
            if docker::engine_ready().await {
                eprintln!("[lightdock] engine is ready");
                self.state.send_replace(true);
                self.touch().await;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Err("engine did not become ready within 30s (is docker installed? run `lightdock setup`)".into())
    }

    pub async fn stop_engine(&self) {
        let mut guard = self.engine.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
        let _ = wsl().args(["--terminate", &self.cfg.distro]).status().await;
        self.state.send_replace(false);
        eprintln!("[lightdock] engine stopped, WSL memory released");
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

/// Accepts Docker CLI connections and forwards them, starting the engine on demand.
pub async fn run_proxy(shared: Arc<Shared>) -> std::io::Result<()> {
    let listener = TcpListener::bind(PROXY_ADDR).await?;
    eprintln!("[lightdock] listening on {PROXY_ADDR}  (set DOCKER_HOST=tcp://{PROXY_ADDR})");
    loop {
        let (client, _) = listener.accept().await?;
        tokio::spawn(handle_client(shared.clone(), client));
    }
}

/// Stops the engine after it has been idle (no clients, no containers) long enough.
pub async fn idle_watcher(shared: Arc<Shared>) {
    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;
        if !shared.is_running() || shared.active_connections() > 0 {
            continue;
        }
        if shared.idle_seconds().await < shared.cfg.idle_timeout.as_secs() {
            continue;
        }
        if docker::has_running_containers().await {
            shared.touch().await; // containers are working, check again later
            continue;
        }
        shared.stop_engine().await;
    }
}
