use clawmon_core::{
    engine::{state_counts, SessionView},
    settings::Settings,
    tasks::{Task, TaskStore, TaskView},
    usage::UsageInfo,
    wsl::{open_terminal, tmux_kill_session, tmux_new_session, tmux_send_keys},
    Detector, Engine, EventKind, SessionEvent,
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

/// Lock rule: never hold two AppState mutexes at once. Take what you need,
/// clone out, drop the guard — the next acquisition is a different one.
struct AppState {
    /// the poll state machine + the last snapshot it computed (the poll
    /// thread is the only writer)
    engine: Mutex<Engine>,
    /// live settings, single owner — the engine takes them by reference on
    /// every update so it never doubles as a config store
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    /// non-fatal problem (e.g. WSL unreachable) for the banner; the sessions
    /// themselves live in the engine's last_views()
    warning: Mutex<Option<String>>,
    /// Latest plan-quota snapshot from the usage loop below.
    last_usage: Mutex<Option<UsageInfo>>,
    /// the task list (FR10): definitions + runtime statuses, reconciled
    /// against every poll's tmux session list
    tasks: Mutex<TaskStore>,
}

#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    sessions: Vec<SessionView>,
    /// non-fatal problem (e.g. WSL unreachable) to show as a banner
    warning: Option<String>,
}

/// Lock a mutex, recovering from poisoning. A panic in one command must not
/// brick the monitor for the rest of the session — this is the one poison
/// policy, used by every site including the window-close handler.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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

// ---- native copy ---------------------------------------------------------
//
// The webview's text lives in ui/i18n.js; native surfaces (notifications,
// tray) are formatted here in the shell. No bundler bridges the two, so the
// language setting drives both sides separately — by design.

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

/// (title, body) for the auto-continue receipt, in the configured language.
fn auto_continue_text(lang: &str, keys: &str, project: &str, count: u32) -> (&'static str, String) {
    if lang == "en" {
        (
            "Auto-continue sent",
            format!("▶ Sent \"{keys}\" to {project} (attempt {count})"),
        )
    } else {
        (
            "已自动继续",
            format!("▶ 已向 {project} 发送「{keys}」（第 {count} 次）"),
        )
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

fn tasks_path(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("tasks.json")
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Update the tray tooltip and the desktop pet with the current light counts.
fn update_status_followers(app: &tauri::AppHandle, sessions: &[SessionView], warning: bool) {
    let mut c = state_counts(sessions);
    c.warning = warning;
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(&format!(
            "clawmon — 🔴{} 🟡{} 🔵{} 🟢{}",
            c.red, c.yellow, c.blue, c.green
        )));
    }
    let _ = app.emit("status", c);
}

/// One monitoring pass: detect the sessions inside WSL, fold the result into
/// the state machine, fire whatever auto-continue has come due and push the
/// outcome to the tray and the windows. Returns the delay until the next pass.
fn poll(app: &tauri::AppHandle, detector: &mut Detector) -> u64 {
    let state = app.state::<AppState>();
    let settings = lock(&state.settings).clone();
    let interval = settings.poll_interval_secs;

    let (sessions, warning, task_views) = match detector.detect(&settings) {
        Ok(snap) => {
            // the task store links its views to claude sessions and keys its
            // liveness on the tmux session list — both come from this poll,
            // so keep a copy before `update` consumes the snapshot
            let raw_sessions = snap.sessions.clone();
            let tmux_names = snap.tmux_sessions.clone();
            let now = snap.now_epoch as i64;
            // `update` counts every attempt it schedules, so sending the keys
            // below cannot be double-booked by a later poll.
            let (views, due, events) = lock(&state.engine).update(snap, &settings);
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
                            let (title, body) = auto_continue_text(
                                &settings.language,
                                &settings.resume_keys,
                                &project,
                                count,
                            );
                            notify(app, title, &body, settings.sound_alerts);
                        }
                    }
                    Err(e) => eprintln!("auto-continue 发送失败 pid={pid}: {e}"),
                }
            }
            // fold the same poll's tmux facts into task statuses (a second
            // lock, taken only after the engine lock is long gone)
            let task_views = {
                let mut store = lock(&state.tasks);
                store.reconcile(&tmux_names, now);
                store.views(&raw_sessions)
            };
            (views, None, task_views)
        }
        // WSL is unreachable: keep showing the last known sessions rather
        // than an empty list, and say so in the banner. Task statuses keep
        // their last state — a dead poll proves nothing either way.
        Err(e) => {
            let sessions = lock(&state.engine).last_views();
            let raw = lock(&state.engine).last_snapshot().to_vec();
            let task_views = lock(&state.tasks).views(&raw);
            (sessions, Some(e), task_views)
        }
    };

    *lock(&state.warning) = warning.clone();
    update_status_followers(app, &sessions, warning.is_some());
    let _ = app.emit("sessions", StatusResponse { sessions, warning });
    let _ = app.emit("tasks", task_views);
    interval
}

/// The monitoring loop lives here rather than in the webview. WebView2
/// throttles timers in hidden windows down to roughly one wake-up per minute,
/// so a JS-driven poll would slow to a crawl exactly when the window sits in
/// the tray or is minimized to the pet — the state the auto-continue has to
/// keep working in. The detector belongs to this thread alone: nothing else
/// talks to it, so it needs no lock.
fn spawn_poll_loop(app: tauri::AppHandle) {
    thread::spawn(move || {
        let mut detector = Detector::new();
        loop {
            let interval = poll(&app, &mut detector);
            thread::sleep(Duration::from_secs(interval.max(1)));
        }
    });
}

/// How often the GLM plan quota is re-queried. The 5-hour window moves
/// slowly; a few minutes of staleness costs nothing and keeps us off the
/// provider's monitor API.
const USAGE_REFRESH_SECS: u64 = 300;

/// Independent loop for the plan-quota chip. Deliberately not part of the
/// detector or its resident process: an HTTP round trip (or a hung one,
/// up to the command timeout) must never couple to the poll cadence. A
/// failing query only means the chip keeps its last good value.
fn spawn_usage_loop(app: tauri::AppHandle) {
    thread::spawn(move || {
        let mut was_enabled = true;
        loop {
            let settings = lock(&app.state::<AppState>().settings).clone();
            if settings.show_glm_usage {
                match clawmon_core::usage::query_usage(&settings) {
                    Ok(info) => {
                        let state = app.state::<AppState>();
                        *lock(&state.last_usage) = Some(info.clone());
                        let _ = app.emit("usage", info);
                    }
                    Err(e) => eprintln!("usage query failed: {e}"),
                }
                was_enabled = true;
            } else if was_enabled {
                // the toggle flipped off: clear what is on screen once
                let state = app.state::<AppState>();
                *lock(&state.last_usage) = None;
                let _ = app.emit("usage", None::<UsageInfo>);
                was_enabled = false;
            }
            thread::sleep(Duration::from_secs(USAGE_REFRESH_SECS));
        }
    });
}

/// The latest snapshot, for the window to render on startup and after being
/// restored from the tray. Detection itself is driven by the poll loop.
#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<StatusResponse, String> {
    let sessions = lock(&state.engine).last_views();
    let warning = lock(&state.warning).clone();
    Ok(StatusResponse { sessions, warning })
}

/// Latest plan-quota snapshot for the header chip's initial paint; the
/// refresh itself runs on the usage loop.
#[tauri::command]
async fn get_usage(state: State<'_, AppState>) -> Result<Option<UsageInfo>, String> {
    Ok(lock(&state.last_usage).clone())
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
    let (pane, keys, distro, lang) = manual_target(&state, pid)?;
    // the key press itself is a blocking WSL round trip (up to the command
    // timeout) — keep it off the async workers. The attempt was already
    // booked before this point, so a poll firing mid-round-trip reschedules
    // instead of sending a second Enter.
    let message =
        tauri::async_runtime::spawn_blocking(move || send_keys(pane, keys, distro, &lang))
            .await
            .map_err(|e| format!("发送任务失败: {e}"))??;
    Ok(message)
}

/// (resume keys, distro, language) — the settings half of any send.
fn send_settings(state: &State<'_, AppState>) -> (Vec<String>, String, String) {
    let s = lock(&state.settings);
    let keys = s
        .resume_keys
        .split_whitespace()
        .map(|k| k.to_string())
        .collect();
    (keys, s.wsl_distro.clone(), s.language.clone())
}

/// The pane a session lives in, from the last snapshot.
fn pane_for(state: &State<'_, AppState>, pid: i32) -> Result<String, String> {
    lock(&state.engine)
        .get_session(pid)
        .and_then(|s| s.tmux.as_ref().map(|t| t.pane.clone()))
        .ok_or_else(|| format!("会话 {pid} 不在 tmux 中，无法控制"))
}

/// The auto-continue path: `update` already booked the attempt under the
/// engine lock, so this only resolves where to send.
fn send_resume(state: &State<'_, AppState>, pid: i32) -> Result<String, String> {
    let (keys, distro, lang) = send_settings(state);
    let pane = pane_for(state, pid)?;
    send_keys(pane, keys, distro, &lang)
}

/// The manual path: resolve the pane AND book the attempt in the same
/// critical section, before any blocking work — the same claim-then-execute
/// invariant `update` applies to auto-sends.
fn manual_target(
    state: &State<'_, AppState>,
    pid: i32,
) -> Result<(String, Vec<String>, String, String), String> {
    let (keys, distro, lang) = send_settings(state);
    let pane = {
        let mut engine = lock(&state.engine);
        let pane = engine
            .get_session(pid)
            .and_then(|s| s.tmux.as_ref().map(|t| t.pane.clone()))
            .ok_or_else(|| format!("会话 {pid} 不在 tmux 中，无法控制"))?;
        engine.claim_send(pid);
        pane
    };
    Ok((pane, keys, distro, lang))
}

/// Blocking `tmux send-keys` inside WSL, plus the receipt message.
fn send_keys(
    pane: String,
    keys: Vec<String>,
    distro: String,
    lang: &str,
) -> Result<String, String> {
    let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    tmux_send_keys(&distro, &pane, &key_refs)?;
    Ok(if lang == "en" {
        format!("Sent {} → {pane}", keys.join(" "))
    } else {
        format!("已发送 {} → {pane}", keys.join(" "))
    })
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    Ok(lock(&state.settings).clone())
}

// ---- task list (FR10) -----------------------------------------------------
//
// One lock at a time, same rule as everywhere else: engine snapshot out
// first, drop the guard, then the task store.

/// Push the current task views to the webview.
fn emit_tasks(app: &tauri::AppHandle, state: &State<'_, AppState>) {
    let raw = lock(&state.engine).last_snapshot().to_vec();
    let views = lock(&state.tasks).views(&raw);
    let _ = app.emit("tasks", views);
}

#[tauri::command]
async fn get_tasks(state: State<'_, AppState>) -> Result<Vec<TaskView>, String> {
    let raw = lock(&state.engine).last_snapshot().to_vec();
    Ok(lock(&state.tasks).views(&raw))
}

#[tauri::command]
async fn add_task(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    title: String,
    cwd: String,
    command: String,
) -> Result<TaskView, String> {
    let raw = lock(&state.engine).last_snapshot().to_vec();
    let view = {
        let mut store = lock(&state.tasks);
        let task = store.add(
            Task {
                title,
                cwd,
                command,
                ..Default::default()
            },
            now_secs(),
        )?;
        store.save()?;
        store
            .views(&raw)
            .into_iter()
            .find(|v| v.id == task.id)
            .expect("just-added task is in the views")
    };
    emit_tasks(&app, &state);
    Ok(view)
}

#[tauri::command]
async fn update_task(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: u64,
    title: String,
    cwd: String,
    command: String,
) -> Result<(), String> {
    {
        let mut store = lock(&state.tasks);
        store.update(
            id,
            Task {
                title,
                cwd,
                command,
                ..Default::default()
            },
        )?;
        store.save()?;
    }
    emit_tasks(&app, &state);
    Ok(())
}

#[tauri::command]
async fn remove_task(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: u64,
) -> Result<(), String> {
    {
        let mut store = lock(&state.tasks);
        store.remove(id)?;
        store.save()?;
    }
    emit_tasks(&app, &state);
    Ok(())
}

/// Launch a task: claim the launch under the store lock (a second click or
/// a poll mid-round-trip is refused), then run the blocking tmux round trip.
/// A failed launch lands in `finished` so the button unblocks.
#[tauri::command]
async fn launch_task(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: u64,
) -> Result<String, String> {
    let (distro, lang) = {
        let s = lock(&state.settings);
        (s.wsl_distro.clone(), s.language.clone())
    };
    let (name, cwd, command) = {
        let mut store = lock(&state.tasks);
        let task = store.claim_launch(id, now_secs())?;
        let _ = store.save(); // lastRunAt survives even an instant-death run
        (task.session_name(), task.cwd, task.command)
    };
    emit_tasks(&app, &state); // "launching" reaches the UI before the trip

    let launch_name = name.clone();
    let launched = tauri::async_runtime::spawn_blocking(move || {
        tmux_new_session(&distro, &launch_name, &cwd, &command)
    })
    .await
    .map_err(|e| format!("启动任务失败: {e}"))?;

    match launched {
        Ok(()) => {
            emit_tasks(&app, &state);
            Ok(if lang == "en" {
                format!("Task started → tmux session {name}")
            } else {
                format!("任务已启动 → tmux 会话 {name}")
            })
        }
        Err(e) => {
            lock(&state.tasks).launch_failed(id, now_secs());
            emit_tasks(&app, &state);
            Err(e)
        }
    }
}

/// Stop a task's tmux session. The stop is booked before the kill round
/// trip (double-click safe); the session dying first is success, not error.
#[tauri::command]
async fn stop_task(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    id: u64,
) -> Result<String, String> {
    let (distro, lang) = {
        let s = lock(&state.settings);
        (s.wsl_distro.clone(), s.language.clone())
    };
    let name = {
        let mut store = lock(&state.tasks);
        let task = store.claim_stop(id, now_secs())?;
        let _ = store.save();
        task.session_name()
    };
    emit_tasks(&app, &state);

    let kill_name = name.clone();
    tauri::async_runtime::spawn_blocking(move || tmux_kill_session(&distro, &kill_name))
        .await
        .map_err(|e| format!("停止任务失败: {e}"))??;
    emit_tasks(&app, &state);
    Ok(if lang == "en" {
        format!("Task stopped ({name})")
    } else {
        format!("任务已停止（{name}）")
    })
}

/// Open a visible terminal attached to the task's tmux session (FR10.4).
/// Spawn-only: the terminal outlives clawmon and monitoring never depends
/// on it.
#[tauri::command]
async fn open_task_terminal(state: State<'_, AppState>, id: u64) -> Result<(), String> {
    let (distro, name) = {
        let store = lock(&state.tasks);
        let task = store
            .tasks()
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("任务 {id} 不存在"))?;
        let name = task.session_name();
        drop(store);
        let distro = lock(&state.settings).wsl_distro.clone();
        (distro, name)
    };
    open_terminal(&distro, &name)
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
    *lock(&state.settings) = settings.clone();
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
        // The drawing facts (ui/pet.html + tauri.conf.json): the crab is
        // 92 px centered in the 110 px window (→ 9 px margin), and the badge
        // juts to 4 px from the edge. 6 keeps the whole badge clickable while
        // still excluding the window's transparent corners. Change these
        // together — this constant is the hit-test half of the pet's geometry.
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

/// A second instance would run a second poll loop and a second pet. A named
/// mutex is the cheapest guard that cannot go stale: the kernel releases it
/// when the owning process dies, unlike a lock file. Windows only.
#[cfg(windows)]
fn ensure_single_instance() {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = "com.statebar.clawmon.single-instance\0"
        .encode_utf16()
        .collect();
    unsafe {
        // The handle is intentionally leaked: it must live as long as the
        // process for the mutex to exist.
        let _ = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            std::process::exit(0);
        }
    }
}

#[cfg(not(windows))]
fn ensure_single_instance() {}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    ensure_single_instance();

    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let path = settings_path(app.handle());
            let settings = Settings::load(&path);
            let tasks = TaskStore::load(&tasks_path(app.handle()));
            app.manage(AppState {
                engine: Mutex::new(Engine::new()),
                settings: Mutex::new(settings.clone()),
                settings_path: path,
                warning: Mutex::new(None),
                last_usage: Mutex::new(None),
                tasks: Mutex::new(tasks),
            });

            // monitoring starts with the app, not with the window: closing to
            // the tray must not pause anything
            spawn_poll_loop(app.handle().clone());

            spawn_usage_loop(app.handle().clone());

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
            let en = settings.language == "en";
            let show = MenuItem::with_id(
                app,
                "show",
                if en {
                    "Show main window"
                } else {
                    "显示主窗口"
                },
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(
                app,
                "quit",
                if en { "Quit" } else { "退出" },
                true,
                None::<&str>,
            )?;
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
                let to_tray = lock(&window.app_handle().state::<AppState>().settings).close_to_tray;
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
            get_usage,
            send_continue,
            get_settings,
            set_settings,
            pet_clicked,
            get_tasks,
            add_task,
            update_task,
            remove_task,
            launch_task,
            stop_task,
            open_task_terminal
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
