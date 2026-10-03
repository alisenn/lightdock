# lightdock

Lightweight, on-demand Docker engine manager for Windows (WSL2). Alternative to Docker Desktop.

- Starts `dockerd` inside WSL2 only when the `docker` CLI connects (lazy start)
- Proxies `127.0.0.1:2375` to the engine
- Shuts the WSL VM down when idle, releasing all RAM

## Usage (Windows)

```powershell
cargo build --release
.\target\release\lightdock.exe setup   # installs Docker Engine in WSL
.\target\release\lightdock.exe         # run the daemon
$env:DOCKER_HOST="tcp://127.0.0.1:2375"
docker run hello-world
```

Env: `LIGHTDOCK_DISTRO` (default `Ubuntu`), `LIGHTDOCK_IDLE_MINUTES` (default `5`).

## Status

MVP core, not yet compiled/tested. TODO: tray icon, panel, named pipe support.
