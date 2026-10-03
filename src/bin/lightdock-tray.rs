//! System tray app: runs the engine proxy + panel server and shows engine state.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::Command;

use lightdock::engine::{self, Config, Shared};
use lightdock::{panel, PANEL_ADDR};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

enum UserEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    Engine(bool),
    Quit,
}

fn icon(running: bool) -> Icon {
    let (r, g, b) = if running { (46, 204, 113) } else { (149, 165, 166) };
    let size = 32u32;
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let alpha = if dx * dx + dy * dy <= 14.0 * 14.0 { 255 } else { 0 };
            rgba.extend_from_slice(&[r, g, b, alpha]);
        }
    }
    Icon::from_rgba(rgba, size, size).expect("valid icon")
}

fn open_url(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("cmd").args(["/C", "start", "", url]).creation_flags(0x0800_0000).spawn();
    }
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = Command::new("xdg-open").arg(url).spawn();
}

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let shared = Shared::new(Config::from_env());
    let token = panel::random_token();
    let panel_url = format!("http://{PANEL_ADDR}/?t={token}");

    rt.spawn(engine::idle_watcher(shared.clone()));
    rt.spawn({
        let shared = shared.clone();
        async move {
            if let Err(e) = engine::run_proxy(shared).await {
                eprintln!("[lightdock] proxy failed: {e}");
            }
        }
    });
    rt.spawn({
        let (shared, token) = (shared.clone(), token.clone());
        async move {
            if let Err(e) = panel::serve(shared, token).await {
                eprintln!("[lightdock] panel failed: {e}");
            }
        }
    });

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    MenuEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |e| {
            let _ = proxy.send_event(UserEvent::Menu(e));
        }
    }));
    TrayIconEvent::set_event_handler(Some({
        let proxy = proxy.clone();
        move |e| {
            let _ = proxy.send_event(UserEvent::Tray(e));
        }
    }));

    // Forward engine up/down changes to the UI thread.
    rt.spawn({
        let proxy = proxy.clone();
        let mut rx = shared.subscribe();
        async move {
            while rx.changed().await.is_ok() {
                let running = *rx.borrow();
                let _ = proxy.send_event(UserEvent::Engine(running));
            }
        }
    });

    let open_item = MenuItem::new("Open panel", true, None);
    let status_item = MenuItem::new("Engine: stopped", false, None);
    let toggle_item = MenuItem::new("Start engine", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &open_item,
        &PredefinedMenuItem::separator(),
        &status_item,
        &toggle_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ]);
    let (open_id, toggle_id, quit_id) = (open_item.id().clone(), toggle_item.id().clone(), quit_item.id().clone());

    let mut tray = None;
    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            // The tray icon must be created once the event loop is running.
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_menu_on_left_click(false)
                    .with_tooltip("LightDock: engine stopped")
                    .with_icon(icon(false))
                    .build()
                    .ok();
            }
            Event::UserEvent(UserEvent::Engine(running)) => {
                status_item.set_text(if running { "Engine: running" } else { "Engine: stopped" });
                toggle_item.set_text(if running { "Stop engine" } else { "Start engine" });
                if let Some(t) = &tray {
                    let _ = t.set_icon(Some(icon(running)));
                    let _ = t.set_tooltip(Some(if running { "LightDock: engine running" } else { "LightDock: engine stopped" }));
                }
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            })) => open_url(&panel_url),
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == open_id {
                    open_url(&panel_url);
                } else if e.id == toggle_id {
                    let shared = shared.clone();
                    rt.spawn(async move {
                        if shared.is_running() {
                            shared.stop_engine().await;
                        } else if let Err(e) = shared.ensure_engine().await {
                            eprintln!("[lightdock] {e}");
                        }
                    });
                } else if e.id == quit_id {
                    let (shared, proxy) = (shared.clone(), proxy.clone());
                    rt.spawn(async move {
                        shared.stop_engine().await;
                        let _ = proxy.send_event(UserEvent::Quit);
                    });
                }
            }
            Event::UserEvent(UserEvent::Quit) => *flow = ControlFlow::Exit,
            _ => {}
        }
    });
}
