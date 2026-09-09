use clawmon_core::{detect, engine::SessionView, settings::Settings, wsl::run_wsl, Engine};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{Manager, State};

struct AppState {
    engine: Mutex<Engine>,
    settings_path: PathBuf,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    sessions: Vec<SessionView>,
    /// non-fatal problem (e.g. WSL unreachable) to show as a banner
    warning: Option<String>,
}

fn settings_path(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("settings.json")
}

/// One poll: detect sessions inside WSL, update the state machine and fire
/// any auto-continue that is due. The frontend calls this on a timer.
#[tauri::command]
async fn get_status(state: State<'_, AppState>) -> Result<StatusResponse, String> {
    let settings = {
        let engine = state.engine.lock().map_err(|e| e.to_string())?;
        engine.settings.clone()
    };

    // the WSL subprocess can take seconds — keep it off the async workers
    let snap = match tauri::async_runtime::spawn_blocking(move || detect(&settings))
        .await
        .map_err(|e| format!("检测任务失败: {e}"))
        .and_then(|r| r)
    {
        Ok(s) => s,
        Err(e) => {
            // keep the UI responsive with the last known session list
            let engine = state.engine.lock().map_err(|e| e.to_string())?;
            let stale = engine.last_views();
            return Ok(StatusResponse {
                sessions: stale,
                warning: Some(e),
            });
        }
    };

    let (_, due) = {
        let mut engine = state.engine.lock().map_err(|e| e.to_string())?;
        engine.update(snap)
    };

    // fire due auto-continues (outside the lock)
    for pid in due {
        if let Err(e) = send_resume(&state, pid) {
            eprintln!("auto-continue 发送失败 pid={pid}: {e}");
        }
        let mut engine = state.engine.lock().map_err(|e| e.to_string())?;
        engine.record_send(pid);
    }

    let engine = state.engine.lock().map_err(|e| e.to_string())?;
    let sessions = engine.last_views();
    Ok(StatusResponse {
        sessions,
        warning: None,
    })
}

/// Send the configured resume keys to a session's tmux pane right now.
#[tauri::command]
async fn send_continue(state: State<'_, AppState>, pid: i32) -> Result<String, String> {
    let r = send_resume(&state, pid)?;
    let mut engine = state.engine.lock().map_err(|e| e.to_string())?;
    engine.record_send(pid);
    Ok(r)
}

fn send_resume(state: &State<'_, AppState>, pid: i32) -> Result<String, String> {
    let (pane, keys, distro) = {
        let engine = state.engine.lock().map_err(|e| e.to_string())?;
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
        (pane, keys, engine.settings.wsl_distro.clone())
    };
    let mut args: Vec<&str> = vec!["tmux", "send-keys", "-t", &pane];
    for k in &keys {
        args.push(k);
    }
    run_wsl(&distro, &args).map(|_| format!("已发送 {} → {pane}", keys.join(" ")))
}

#[tauri::command]
async fn get_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    let engine = state.engine.lock().map_err(|e| e.to_string())?;
    Ok(engine.settings.clone())
}

#[tauri::command]
async fn set_settings(state: State<'_, AppState>, settings: Settings) -> Result<(), String> {
    {
        let mut engine = state.engine.lock().map_err(|e| e.to_string())?;
        engine.settings = settings.clone();
    }
    settings.save(&state.settings_path)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let path = settings_path(&app.handle());
            let settings = Settings::load(&path);
            app.manage(AppState {
                engine: Mutex::new(Engine::new(settings)),
                settings_path: path,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_status,
            send_continue,
            get_settings,
            set_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
