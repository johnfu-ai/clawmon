use crate::detector::{RawSession, RawStatus};
use crate::settings::Settings;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Green,
    Yellow,
    Red,
}

/// Per-session tracking kept between polls (block episode bookkeeping).
#[derive(Debug, Clone, Default)]
struct Tracked {
    blocked_since: Option<i64>,
    sends: u32,
    last_send_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub pid: i32,
    /// display name: basename of cwd
    pub project: String,
    pub cwd: String,
    pub tty: String,
    /// e.g. "work:1.0" when the session lives inside tmux
    pub tmux_label: Option<String>,
    pub session_id: String,
    pub state: SessionState,
    /// human readable Chinese status label
    pub label: String,
    pub idle_sec: Option<i64>,
    pub preview: String,
    /// tmux pane id, present only when we can control this session
    pub pane: Option<String>,
    pub controllable: bool,
    /// epoch seconds when this block episode started (red only)
    pub blocked_since: Option<i64>,
    /// seconds until the next auto-send fires (red + auto-continue only)
    pub remaining_sec: Option<i64>,
    pub sends: u32,
    pub last_send_at: Option<i64>,
}

pub struct Engine {
    pub settings: Settings,
    tracked: HashMap<i32, Tracked>,
    last_snapshot: Vec<RawSession>,
    last_views: Vec<SessionView>,
    /// clock (WSL epoch) of the last successful update — all scheduling
    /// math uses this so tests with synthetic timestamps stay consistent
    last_now: i64,
}

fn basename(p: &str) -> String {
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        return "/".into();
    }
    p.rsplit('/').next().unwrap_or(p).to_string()
}

fn classify(s: &RawSession, st: &Settings) -> (SessionState, &'static str) {
    let idle = s.idle_sec.unwrap_or(i64::MAX);
    match s.last_type.as_str() {
        // transcript activity within the green window → actively working
        _ if idle < st.idle_green_secs => (SessionState::Green, "运行中"),
        // a tool_use block is the last transcript activity: the tool result
        // is only appended when the tool *finishes*, so claude is
        // legitimately busy (e.g. a long Bash command) — never blocked
        _ if s.tool_running => (SessionState::Yellow, "工具运行中"),
        // claude finished its turn and is waiting for the human
        "assistant" => (SessionState::Yellow, "等待输入"),
        // waiting for claude to respond — if it stays this way too long the
        // API side is likely exhausted (usage limit pause)
        _ => {
            if idle >= st.blocked_after_secs {
                (SessionState::Red, "疑似 API 超时")
            } else {
                (SessionState::Yellow, "等待响应")
            }
        }
    }
}

impl Engine {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            tracked: HashMap::new(),
            last_snapshot: Vec::new(),
            last_views: Vec::new(),
            last_now: 0,
        }
    }

    /// The views computed by the last successful `update` — used to keep the
    /// UI populated when WSL becomes unreachable.
    pub fn last_views(&self) -> Vec<SessionView> {
        self.last_views.clone()
    }

    pub fn get_session(&self, pid: i32) -> Option<&RawSession> {
        self.last_snapshot.iter().find(|s| s.pid == pid)
    }

    /// Fold a fresh snapshot into the state machine.
    /// Returns the views for the UI plus the pids whose auto-continue fires
    /// *now* (the caller performs the actual key sending, then calls
    /// `record_send` for each).
    pub fn update(&mut self, snap: RawStatus) -> (Vec<SessionView>, Vec<i32>) {
        self.last_snapshot = snap.sessions.clone();
        let now = snap.now_epoch as i64;
        self.last_now = now;
        let mut views = Vec::new();
        let mut due = Vec::new();

        let live: Vec<i32> = snap.sessions.iter().map(|s| s.pid).collect();
        self.tracked.retain(|k, _| live.contains(k));

        for s in &snap.sessions {
            let (state, label) = classify(s, &self.settings);
            let t = self.tracked.entry(s.pid).or_default();

            match state {
                SessionState::Red => {
                    if t.blocked_since.is_none() {
                        t.blocked_since = Some(now);
                    }
                }
                _ => {
                    // recovered or never was blocked — reset the episode
                    if t.blocked_since.is_some() {
                        *t = Tracked::default();
                    }
                }
            }

            let controllable = s.tmux.is_some();
            let mut remaining: Option<i64> = None;
            if state == SessionState::Red && self.settings.auto_continue {
                let next_at = if t.sends == 0 {
                    t.blocked_since.unwrap_or(now) + self.settings.wait_secs as i64
                } else {
                    t.last_send_at.unwrap_or(now) + self.settings.retry_interval_secs as i64
                };
                remaining = Some((next_at - now).max(0));
                if next_at <= now && controllable && t.sends < self.settings.max_sends {
                    due.push(s.pid);
                    remaining = Some(0);
                }
            }

            views.push(SessionView {
                pid: s.pid,
                project: basename(&s.cwd),
                cwd: s.cwd.clone(),
                tty: s.tty.clone(),
                tmux_label: s
                    .tmux
                    .as_ref()
                    .map(|t| format!("{}:{}", t.session, t.window)),
                session_id: s.session_id.clone(),
                state,
                label: label.to_string(),
                idle_sec: s.idle_sec,
                preview: s.preview.clone(),
                pane: s.tmux.as_ref().map(|t| t.pane.clone()),
                controllable,
                blocked_since: t.blocked_since,
                remaining_sec: remaining,
                sends: t.sends,
                last_send_at: t.last_send_at,
            });
        }

        views.sort_by(|a, b| {
            let rank = |s: &SessionView| match s.state {
                SessionState::Red => 0,
                SessionState::Yellow => 1,
                SessionState::Green => 2,
            };
            rank(a).cmp(&rank(b)).then(a.pid.cmp(&b.pid))
        });
        self.last_views = views.clone();
        (views, due)
    }

    /// Mark that resume keys were sent to `pid` (manual or automatic).
    pub fn record_send(&mut self, pid: i32) {
        let now = self.last_now;
        let t = self.tracked.entry(pid).or_default();
        t.sends += 1;
        t.last_send_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pid: i32, last_type: &str, idle: i64) -> RawSession {
        RawSession {
            pid,
            cwd: "/home/john/statebar".into(),
            tty: "/dev/pts/2".into(),
            tmux: Some(crate::detector::TmuxInfo {
                pane: "%0".into(),
                session: "work".into(),
                window: "0.0".into(),
            }),
            transcript: None,
            session_id: "abc".into(),
            last_type: last_type.into(),
            last_ts: String::new(),
            idle_sec: Some(idle),
            preview: "做点什么".into(),
            tool_running: false,
        }
    }

    fn snap(now: i64, sessions: Vec<RawSession>) -> RawStatus {
        RawStatus {
            now: String::new(),
            now_epoch: now as f64,
            sessions,
        }
    }

    #[test]
    fn green_when_active() {
        let mut e = Engine::new(Settings::default());
        let (v, due) = e.update(snap(1000, vec![session(1, "assistant", 10)]));
        assert_eq!(v[0].state, SessionState::Green);
        assert!(due.is_empty());
    }

    #[test]
    fn yellow_waiting_for_input() {
        let mut e = Engine::new(Settings::default());
        let (v, _) = e.update(snap(1000, vec![session(1, "assistant", 300)]));
        assert_eq!(v[0].state, SessionState::Yellow);
        assert_eq!(v[0].label, "等待输入");
    }

    #[test]
    fn yellow_then_red_when_waiting_on_api() {
        let mut e = Engine::new(Settings::default());
        // default blocked_after_secs = 300
        let (v, _) = e.update(snap(1000, vec![session(1, "user", 240)]));
        assert_eq!(v[0].state, SessionState::Yellow);

        let (v, _) = e.update(snap(1300, vec![session(1, "user", 540)]));
        assert_eq!(v[0].state, SessionState::Red);
        assert_eq!(v[0].blocked_since, Some(1300));
    }

    #[test]
    fn auto_continue_fires_after_wait() {
        let mut st = Settings::default();
        st.wait_secs = 100;
        let mut e = Engine::new(st);
        let t0 = 10_000;
        let (v, due) = e.update(snap(t0, vec![session(1, "user", 1000)]));
        assert_eq!(v[0].state, SessionState::Red);
        assert!(due.is_empty());
        assert_eq!(v[0].remaining_sec, Some(100));

        let (v, due) = e.update(snap(t0 + 99, vec![session(1, "user", 1099)]));
        assert!(due.is_empty());
        assert_eq!(v[0].remaining_sec, Some(1));

        let (_, due) = e.update(snap(t0 + 100, vec![session(1, "user", 1100)]));
        assert_eq!(due, vec![1]);

        e.record_send(1);
        let (v, _) = e.update(snap(t0 + 101, vec![session(1, "user", 1101)]));
        assert_eq!(v[0].sends, 1);
        // retry scheduled retry_interval_secs (600) after the send
        assert_eq!(v[0].remaining_sec, Some(599));
    }

    #[test]
    fn max_sends_cap() {
        let mut st = Settings::default();
        st.wait_secs = 0;
        st.retry_interval_secs = 0;
        st.max_sends = 2;
        let mut e = Engine::new(st);
        let t0 = 10_000;
        for i in 0..3 {
            let (_, due) = e.update(snap(t0 + i, vec![session(1, "user", 5000)]));
            for pid in due {
                e.record_send(pid);
            }
        }
        let (v, _) = e.update(snap(t0 + 10, vec![session(1, "user", 5010)]));
        assert_eq!(v[0].sends, 2);
    }

    #[test]
    fn recovers_to_green_resets_episode() {
        let mut e = Engine::new(Settings::default());
        let t0 = 10_000;
        let (_, _) = e.update(snap(t0, vec![session(1, "user", 1000)]));
        let (v, _) = e.update(snap(t0 + 1, vec![session(1, "user", 1001)]));
        assert!(v[0].blocked_since.is_some());

        let (v, _) = e.update(snap(t0 + 2, vec![session(1, "assistant", 2)]));
        assert_eq!(v[0].state, SessionState::Green);
        assert!(v[0].blocked_since.is_none());

        // turns red again → fresh episode
        let (v, _) = e.update(snap(t0 + 3, vec![session(1, "user", 1000)]));
        assert_eq!(v[0].blocked_since, Some(t0 + 3));
    }

    #[test]
    fn not_controllable_without_tmux() {
        let mut s = session(1, "user", 1000);
        s.tmux = None;
        let mut st = Settings::default();
        st.wait_secs = 0;
        let mut e = Engine::new(st);
        let t0 = 10_000;
        let (v, due) = e.update(snap(t0, vec![s]));
        assert!(!v[0].controllable);
        assert!(due.is_empty()); // never auto-sends to an uncontrolled session
    }

    #[test]
    fn tool_running_never_red() {
        // a long-running tool (assistant entry whose last block is tool_use)
        // must not be classified as blocked, no matter how long it runs
        let mut st = Settings::default();
        st.wait_secs = 0;
        let mut e = Engine::new(st);
        let mut s = session(1, "assistant", 7200); // 2h "idle" while tool runs
        s.tool_running = true;
        let t0 = 10_000;
        let (v, due) = e.update(snap(t0, vec![s]));
        assert_eq!(v[0].state, SessionState::Yellow);
        assert_eq!(v[0].label, "工具运行中");
        assert!(due.is_empty());
        assert!(v[0].blocked_since.is_none());
    }

    #[test]
    fn sorts_red_first() {
        let mut e = Engine::new(Settings::default());
        let (v, _) = e.update(snap(
            1000,
            vec![session(5, "assistant", 10), session(9, "user", 900)],
        ));
        assert_eq!(v[0].pid, 9);
        assert_eq!(v[1].pid, 5);
    }
}
