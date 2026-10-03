//! Console entry point: `lightdock [run|setup]`. For the tray app see `lightdock-tray`.

use lightdock::engine::{self, wsl, Config, Shared};
use lightdock::{panel, PANEL_ADDR, PROXY_ADDR};

async fn run_headless() -> Result<(), Box<dyn std::error::Error>> {
    let shared = Shared::new(Config::from_env());
    let token = panel::random_token();
    tokio::spawn(engine::idle_watcher(shared.clone()));
    let panel_task = tokio::spawn(panel::serve(shared.clone(), token.clone()));
    eprintln!("[lightdock] panel: http://{PANEL_ADDR}/?t={token}");

    tokio::select! {
        r = engine::run_proxy(shared.clone()) => r?,
        r = panel_task => { r??; }
        _ = tokio::signal::ctrl_c() => {}
    }
    eprintln!("[lightdock] shutting down");
    shared.stop_engine().await;
    Ok(())
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
    println!("\nDone. Start `lightdock-tray` (or `lightdock`) and set:");
    println!("  setx DOCKER_HOST tcp://{PROXY_ADDR}");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        None | Some("run") => run_headless().await,
        Some("setup") => setup().await,
        Some(_) => {
            println!("usage: lightdock [run|setup]\n\nenv: LIGHTDOCK_DISTRO (default Ubuntu), LIGHTDOCK_IDLE_MINUTES (default 5)");
            Ok(())
        }
    }
}
