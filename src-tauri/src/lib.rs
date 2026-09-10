use clawmon_core::{detect, engine::SessionView, settings::Settings, wsl::run_wsl, Engine};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State, WindowEvent,
};

struct AppState {
    engine: Mutex<Engine>,
    settings_path: PathBuf,
    /// The latest snapshot, served to the webview on demand. The window is a
    /// viewer only: the poll loop below keeps running (and keeps pushing
    /// events at it) whether or not there is a window on screen.
    last: Mutex<StatusResponse>,
}

#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    sessions: Vec<SessionView>,
    /// non-fatal problem (e.g. WSL unreachable) to show as a banner
    warning: Option<String>,
}

/// Lock a mutex, recovering from poisoning. A panic in one command must not
/// brick the monitor for the rest of the session.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// (red, yellow, green) session counts, pushed to the tray tooltip and the pet.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusCounts {
    red: u32,
    yellow: u32,
    green: u32,
    warning: bool,
}

fn state_counts(sessions: &[SessionView]) -> StatusCounts {
    let mut c = StatusCounts {
        red: 0,
        yellow: 0,
        green: 0,
        warning: false,
    };
    for s in sessions {
        match s.state {
            clawmon_core::engine::SessionState::Red => c.red += 1,
            clawmon_core::engine::SessionState::Yellow => c.yellow += 1,
            clawmon_core::engine::SessionState::Green => c.green += 1,
        }
    }
    c
}

fn settings_path(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("settings.json")
}

/// Update the tray tooltip and the desktop pet with the current light counts.
fn update_status_followers(app: &tauri::AppHandle, sessions: &[SessionView], warning: bool) {
    let c = state_counts(sessions);
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(&format!(
            "clawmon — 🔴{} 🟡{} 🟢{}",
            c.red, c.yellow, c.green
        )));
    }
    let _ = app.emit(
        "status",
        StatusCounts {
            warning,
            ..c.clone()
        },
    );
}

/// One monitoring pass: detect the sessions inside WSL, fold the result into
/// the state machine, fire whatever auto-continue has come due and push the
/// outcome to the tray and the windows. Returns the delay until the next pass.
fn poll(app: &tauri::AppHandle) -> u64 {
    let state = app.state::<AppState>();
    let settings = lock(&state.engine).settings.clone();
    let interval = settings.poll_interval_secs;

    let (sessions, warning) = match detect(&settings) {
        Ok(snap) => {
            // `update` counts every attempt it schedules, so sending the keys
            // below cannot be double-booked by a later poll.
            let due = lock(&state.engine).update(snap).1;
            for pid in due {
                if let Err(e) = send_resume(&state, pid) {
                    eprintln!("auto-continue 发送失败 pid={pid}: {e}");
                }
            }
            (lock(&state.engine).last_views(), None)
        }
        // WSL is unreachable: keep showing the last known sessions rather
        // than an empty list, and say so in the banner
        Err(e) => (lock(&state.engine).last_views(), Some(e)),
    };

    let response = StatusResponse { sessions, warning };
    update_status_followers(app, &response.sessions, response.warning.is_some());
    *lock(&state.last) = response.clone();
    let _ = app.emit("sessions", response);
    interval
}

/// The monitoring loop lives here rather than in the webview. WebView2
/// throttles timers in hidden windows down to roughly one wake-up per minute,
/// so a JS-driven poll would slow to a crawl exactly when the window sits in
/// the tray or is minimized to the pet — the state the auto-continue has to
/// keep working in.
fn spawn_poll_loop(app: tauri::AppHandle) {
    thread::spawn(move || loop {
        let interval = poll(&app);
        thread::sleep(Duration::from_secs(interval.max(1)));
    });
}

/// The latest snapshot, for the window to render on startup and after being
/// restored from the tray. Detection itself is driven by the poll loop.
#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<StatusResponse, String> {
    Ok(lock(&state.last).clone())
}

/// The desktop pet was clicked: bring the main window back and dismiss the pet.
#[tauri::command]
fn pet_clicked(app: tauri::AppHandle) {
    show_main_window(&app);
}

/// Send the configured resume keys to a session's tmux pane right now.
#[tauri::command]
async fn send_continue(state: State<'_, AppState>, pid: i32) -> Result<String, String> {
    let (pane, keys, distro) = resume_target(&state, pid)?;
    // the key press itself is a blocking WSL round trip (up to the command
    // timeout) — keep it off the async workers
    let message = tauri::async_runtime::spawn_blocking(move || send_keys(pane, keys, distro))
        .await
        .map_err(|e| format!("发送任务失败: {e}"))??;
    lock(&state.engine).record_send(pid);
    Ok(message)
}

/// What to send and where, resolved from the last snapshot.
fn resume_target(
    state: &State<'_, AppState>,
    pid: i32,
) -> Result<(String, Vec<String>, String), String> {
    let engine = lock(&state.engine);
    let pane = engine
        .get_session(pid)
        .and_then(|s| s.tmux.as_ref().map(|t| t.pane.clone()))
        .ok_or_else(|| format!("会话 {pid} 不在 tmux 中，无法控制"))?;
    let keys: Vec<String> = engine
        .settings
        .resume_keys
        .split_whitespace()
        .map(|s| s.to_string())
        .collect();
    Ok((pane, keys, engine.settings.wsl_distro.clone()))
}

/// Blocking `tmux send-keys` inside WSL. Keys are passed as argv entries, never
/// through a shell.
fn send_keys(pane: String, keys: Vec<String>, distro: String) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["tmux", "send-keys", "-t", &pane];
    for k in &keys {
        args.push(k);
    }
    run_wsl(&distro, &args).map(|_| format!("已发送 {} → {pane}", keys.join(" ")))
}

/// The auto-continue path: already runs on the poll thread, so it can block.
fn send_resume(state: &State<'_, AppState>, pid: i32) -> Result<String, String> {
    let (pane, keys, distro) = resume_target(state, pid)?;
    send_keys(pane, keys, distro)
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    Ok(lock(&state.engine).settings.clone())
}

#[tauri::command]
async fn set_settings(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    // the webview is not the only writer of settings.json, so clamp before
    // anything starts acting on the values
    let settings = settings.sanitize();
    settings.save(&state.settings_path)?;
    lock(&state.engine).settings = settings;
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(pet) = app.get_webview_window("pet") {
        let _ = pet.hide();
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Minimizing the main window turns it into the desktop pet: hide the window
/// (taskbar entry goes with it), let the cat take over — without stealing
/// focus from whatever the user switched to.
fn minimize_to_pet(app: &tauri::AppHandle) {
    if let Some(pet) = app.get_webview_window("pet") {
        #[cfg(windows)]
        {
            use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE};
            if let Ok(h) = pet.hwnd() {
                unsafe {
                    let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = pet.show();
        }
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let path = settings_path(app.handle());
            let settings = Settings::load(&path);
            app.manage(AppState {
                engine: Mutex::new(Engine::new(settings)),
                settings_path: path,
                last: Mutex::new(StatusResponse::default()),
            });

            // monitoring starts with the app, not with the window: closing to
            // the tray must not pause anything
            spawn_poll_loop(app.handle().clone());

            // park the pet in the bottom-right corner of the primary monitor
            // (raised ~90px so it clears the taskbar)
            if let Some(pet) = app.get_webview_window("pet") {
                let size = pet
                    .outer_size()
                    .unwrap_or(tauri::PhysicalSize::new(110, 110));
                if let Ok(Some(mon)) = app.primary_monitor() {
                    let m = mon.size();
                    let p = mon.position();
                    let _ = pet.set_position(tauri::PhysicalPosition::new(
                        p.x + m.width as i32 - size.width as i32 - 24,
                        p.y + m.height as i32 - size.height as i32 - 96,
                    ));
                }
            }

            // system tray: keeps the monitor alive with the window closed
            let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("main")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("clawmon")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                let to_tray = window
                    .app_handle()
                    .state::<AppState>()
                    .engine
                    .lock()
                    .map(|e| e.settings.close_to_tray)
                    .unwrap_or(true);
                if to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
            // Windows reports a minimize as a move to (-32000, -32000); confirm
            // with the OS, then trade the window for the desktop pet
            WindowEvent::Moved(_) | WindowEvent::Resized(_)
                if window.label() == "main" && window.is_minimized().unwrap_or(false) =>
            {
                minimize_to_pet(window.app_handle());
            }
            // the window came back some other way (tray, taskbar) — cat can nap
            WindowEvent::Focused(true) if window.label() == "main" => {
                if let Some(pet) = window.app_handle().get_webview_window("pet") {
                    let _ = pet.hide();
                }
            }
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            send_continue,
            get_settings,
            set_settings,
            pet_clicked
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
