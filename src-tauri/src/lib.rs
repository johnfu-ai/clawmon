use clawmon_core::{
    engine::SessionView, settings::Settings, wsl::run_wsl, Detector, Engine, EventKind,
    SessionEvent,
};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State, WindowEvent,
};
use tauri_plugin_notification::NotificationExt;

struct AppState {
    engine: Mutex<Engine>,
    /// the resident detection process, kept across polls
    detector: Mutex<Detector>,
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

/// System alert sound. Toasts already carry their own chime on Windows,
/// but an explicit beep stays audible even when notifications are muted or
/// the banner misses the moment.
#[cfg(windows)]
fn play_alert() {
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::WindowsAndMessaging::MB_ICONEXCLAMATION;
    unsafe {
        let _ = MessageBeep(MB_ICONEXCLAMATION);
    }
}

#[cfg(not(windows))]
fn play_alert() {}

/// One desktop notification (plus its sound, if enabled).
fn notify(app: &tauri::AppHandle, title: &str, body: &str, sound: bool) {
    if sound {
        play_alert();
    }
    let _ = app.notification().builder().title(title).body(body).show();
}

/// (title, body) for a session event, in the configured language.
fn event_text(lang: &str, kind: EventKind, project: &str) -> (&'static str, String) {
    let en = lang == "en";
    match kind {
        EventKind::TurnedRed => (
            if en {
                "Session stuck"
            } else {
                "会话疑似卡死"
            },
            if en {
                format!("🔴 {project} looks blocked — will auto-continue as configured")
            } else {
                format!("🔴 {project} 疑似 API 超时，将按设置自动继续")
            },
        ),
        EventKind::Recovered => (
            if en {
                "Session recovered"
            } else {
                "会话已恢复"
            },
            if en {
                format!("🟢 {project} is running again")
            } else {
                format!("🟢 {project} 已恢复运行")
            },
        ),
        EventKind::TurnEnd => (
            if en { "Turn finished" } else { "回合结束" },
            if en {
                format!("✅ {project} finished its turn and waits for your input")
            } else {
                format!("✅ {project} 本轮任务完成，等待输入")
            },
        ),
        EventKind::Exited => (
            if en {
                "Session ended"
            } else {
                "会话已结束"
            },
            if en {
                format!("⏹ the claude process for {project} has exited")
            } else {
                format!("⏹ {project} 的 claude 进程已退出")
            },
        ),
    }
}

/// Turn a state transition into a notification, honoring the per-kind
/// switches. `settings` is the copy this poll started with, so flipping a
/// switch takes effect on the next poll at the latest.
fn dispatch_event(app: &tauri::AppHandle, settings: &Settings, ev: SessionEvent) {
    let enabled = match ev.kind {
        EventKind::TurnedRed => settings.notify_red,
        EventKind::Recovered => settings.notify_recovered,
        EventKind::TurnEnd => settings.notify_turn_end,
        EventKind::Exited => settings.notify_exit,
    };
    if !enabled {
        return;
    }
    let (title, body) = event_text(&settings.language, ev.kind, &ev.project);
    notify(app, title, &body, settings.sound_alerts);
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

    let (sessions, warning) = match lock(&state.detector).detect(&settings) {
        Ok(snap) => {
            // `update` counts every attempt it schedules, so sending the keys
            // below cannot be double-booked by a later poll.
            let (views, due, events) = lock(&state.engine).update(snap);
            for ev in events {
                dispatch_event(app, &settings, ev);
            }
            for pid in due {
                match send_resume(&state, pid) {
                    Ok(_) => {
                        if settings.notify_continue {
                            // the attempt was already counted by `update`,
                            // so the fresh views carry the right number
                            let count = views
                                .iter()
                                .find(|v| v.pid == pid)
                                .map(|v| v.sends)
                                .unwrap_or(1);
                            let project = views
                                .iter()
                                .find(|v| v.pid == pid)
                                .map(|v| v.project.clone())
                                .unwrap_or_default();
                            let en = settings.language == "en";
                            let (title, body) = if en {
                                (
                                    "Auto-continue sent",
                                    format!(
                                        "▶ Sent \"{}\" to {project} (attempt {count})",
                                        settings.resume_keys
                                    ),
                                )
                            } else {
                                (
                                    "已自动继续",
                                    format!(
                                        "▶ 已向 {project} 发送「{}」（第 {count} 次）",
                                        settings.resume_keys
                                    ),
                                )
                            };
                            notify(app, title, &body, settings.sound_alerts);
                        }
                    }
                    Err(e) => eprintln!("auto-continue 发送失败 pid={pid}: {e}"),
                }
            }
            (views, None)
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

/// The desktop pet was clicked: bring the main window back. The pet itself
/// stays on screen — it is the persistent status indicator and launcher.
#[tauri::command]
fn pet_clicked(app: tauri::AppHandle) {
    show_main_window(&app);
}

/// Send the configured resume keys to a session's tmux pane right now.
#[tauri::command]
async fn send_continue(state: State<'_, AppState>, pid: i32) -> Result<String, String> {
    let (pane, keys, distro, lang) = resume_target(&state, pid)?;
    // the key press itself is a blocking WSL round trip (up to the command
    // timeout) — keep it off the async workers
    let message =
        tauri::async_runtime::spawn_blocking(move || send_keys(pane, keys, distro, &lang))
            .await
            .map_err(|e| format!("发送任务失败: {e}"))??;
    lock(&state.engine).record_send(pid);
    Ok(message)
}

/// What to send and where, resolved from the last snapshot.
fn resume_target(
    state: &State<'_, AppState>,
    pid: i32,
) -> Result<(String, Vec<String>, String, String), String> {
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
    Ok((
        pane,
        keys,
        engine.settings.wsl_distro.clone(),
        engine.settings.language.clone(),
    ))
}

/// Blocking `tmux send-keys` inside WSL. Keys are passed as argv entries, never
/// through a shell.
fn send_keys(
    pane: String,
    keys: Vec<String>,
    distro: String,
    lang: &str,
) -> Result<String, String> {
    let mut args: Vec<&str> = vec!["tmux", "send-keys", "-t", &pane];
    for k in &keys {
        args.push(k);
    }
    run_wsl(&distro, &args).map(|_| {
        if lang == "en" {
            format!("Sent {} → {pane}", keys.join(" "))
        } else {
            format!("已发送 {} → {pane}", keys.join(" "))
        }
    })
}

/// The auto-continue path: already runs on the poll thread, so it can block.
fn send_resume(state: &State<'_, AppState>, pid: i32) -> Result<String, String> {
    let (pane, keys, distro, lang) = resume_target(state, pid)?;
    send_keys(pane, keys, distro, &lang)
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    Ok(lock(&state.engine).settings.clone())
}

#[tauri::command]
async fn set_settings(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    settings: Settings,
) -> Result<(), String> {
    // the webview is not the only writer of settings.json, so clamp before
    // anything starts acting on the values
    let settings = settings.sanitize();
    settings.save(&state.settings_path)?;
    lock(&state.engine).settings = settings.clone();
    // the pet and the main window pick up language changes from this
    let _ = app.emit("settings", settings);
    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Show the pet without stealing focus from whatever the user is doing.
/// The pet is a permanent fixture: it appears at startup and survives every
/// show/hide of the main window (only quitting the app removes it).
fn show_pet(app: &tauri::AppHandle) {
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
}

/// Minimizing the main window hides it (taskbar entry goes with it) and hands
/// the monitoring face entirely to the pet.
fn minimize_to_pet(app: &tauri::AppHandle) {
    show_pet(app);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

/// Toggle hit-test transparency by flipping ONLY `WS_EX_TRANSPARENT` on the
/// pet window. Tauri's `set_ignore_cursor_events` rewrites the whole extended
/// style and drops `WS_EX_LAYERED` when clearing the flag — the very style a
/// transparent WebView2 window renders through — which blanks the pet the
/// moment the cursor enters it. The pet stays layered for its whole lifetime,
/// so toggling the single transparency bit switches between click-through and
/// interactive without ever disturbing rendering.
#[cfg(windows)]
fn set_pet_click_through(pet: &tauri::WebviewWindow, through: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_TRANSPARENT,
    };
    if let Ok(hwnd) = pet.hwnd() {
        unsafe {
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let style = if through {
                style | WS_EX_TRANSPARENT.0 as isize
            } else {
                style & !(WS_EX_TRANSPARENT.0 as isize)
            };
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
        }
    }
}

/// Keep the pet window click-through except over the crab itself. The window
/// is a square slightly larger than the drawing, and its transparent corners
/// would swallow clicks meant for whatever sits in the screen corner. A
/// cheap cursor poll toggles window-level hit-test transparency; while a
/// mouse button is down the toggling pauses, so an active drag of the crab
/// is never dropped mid-move.
#[cfg(windows)]
fn spawn_pet_hit_test(app: tauri::AppHandle) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    thread::spawn(move || {
        // the crab fills ~92 of the 110 px window; the badge juts a little
        // further into the margin
        const INSET: i32 = 6;
        let mut through: Option<bool> = None;
        loop {
            thread::sleep(Duration::from_millis(150));
            let Some(pet) = app.get_webview_window("pet") else {
                continue;
            };
            // high bit set = currently pressed (i16 negative)
            let dragging = unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 };
            let mut click_through = true;
            if !dragging && pet.is_visible().unwrap_or(false) {
                if let (Ok(pos), Ok(size)) = (pet.outer_position(), pet.outer_size()) {
                    let mut pt = POINT::default();
                    if unsafe { GetCursorPos(&mut pt) }.is_ok() {
                        let inside = pt.x >= pos.x + INSET
                            && pt.x < pos.x + size.width as i32 - INSET
                            && pt.y >= pos.y + INSET
                            && pt.y < pos.y + size.height as i32 - INSET;
                        click_through = !inside;
                    }
                }
            }
            if through != Some(click_through) {
                set_pet_click_through(&pet, click_through);
                through = Some(click_through);
            }
        }
    });
}

#[cfg(not(windows))]
fn spawn_pet_hit_test(_app: tauri::AppHandle) {}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let path = settings_path(app.handle());
            let settings = Settings::load(&path);
            app.manage(AppState {
                engine: Mutex::new(Engine::new(settings.clone())),
                detector: Mutex::new(Detector::new()),
                settings_path: path,
                last: Mutex::new(StatusResponse::default()),
            });

            // monitoring starts with the app, not with the window: closing to
            // the tray must not pause anything
            spawn_poll_loop(app.handle().clone());

            spawn_pet_hit_test(app.handle().clone());

            // park the pet in the bottom-right corner of the primary monitor
            // (raised ~90px so it clears the taskbar) and put it on screen for
            // good: it is the always-on status face, not a minimize artifact
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
            show_pet(app.handle());

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
