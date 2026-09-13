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

/// What happened to a session between two polls — the hooks the shell layer
/// turns into desktop notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// entered a red (blocked) episode
    TurnedRed,
    /// left a red episode without our help (usage window reset, etc.)
    Recovered,
    /// finished its turn and is now waiting for the user's next instruction
    TurnEnd,
    /// the claude process is gone
    Exited,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionEvent {
    pub pid: i32,
    pub kind: EventKind,
    /// basename of the session cwd, ready for message formatting
    pub project: String,
}

/// Per-session tracking kept between polls (block episode bookkeeping).
#[derive(Debug, Clone, Default)]
struct Tracked {
    blocked_since: Option<i64>,
    sends: u32,
    last_send_at: Option<i64>,
    /// state at the previous poll, for edge detection
    last_state: Option<SessionState>,
    /// the previous poll already saw this session "waiting for input"
    was_waiting_input: bool,
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
    /// tokens consumed by this session so far (display only — never feeds
    /// the red/yellow/green classification)
    pub usage: Option<crate::detector::RawUsage>,
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
    // transcript activity within the green window → actively working
    if idle < st.idle_green_secs {
        return (SessionState::Green, "运行中");
    }
    // Without a transcript we can vouch for, "idle for six hours" means
    // nothing: either claude has not written anything yet, or the file
    // belongs to a different session. Never call that blocked — the cost of a
    // false positive is pressing Enter in an innocent terminal.
    if s.transcript.is_none() {
        return (SessionState::Yellow, "未找到记录");
    }
    if !s.transcript_live {
        return (SessionState::Yellow, "记录未就绪");
    }
    // a tool_use block is the last transcript activity: the tool result is
    // only appended when the tool *finishes*, so claude is legitimately busy
    // (e.g. a long Bash command) — never blocked
    if s.tool_running {
        return (SessionState::Yellow, "工具运行中");
    }
    // claude finished its turn and is waiting for the human
    if s.last_type == "assistant" {
        return (SessionState::Yellow, "等待输入");
    }
    // waiting for claude to respond — if it stays this way too long the API
    // side is likely exhausted (usage limit pause)
    if idle >= st.blocked_after_secs {
        (SessionState::Red, "疑似 API 超时")
    } else {
        (SessionState::Yellow, "等待响应")
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
    ///
    /// Returns the views for the UI, the pids whose auto-continue fires *now*,
    /// and the state transitions that happened since the previous poll. A
    /// fired attempt is already counted against the episode (see below), so
    /// the caller only has to send the keys — it must not call `record_send`
    /// again for these pids.
    pub fn update(&mut self, snap: RawStatus) -> (Vec<SessionView>, Vec<i32>, Vec<SessionEvent>) {
        let prev = std::mem::take(&mut self.last_snapshot);
        self.last_snapshot = snap.sessions.clone();
        let now = snap.now_epoch as i64;
        self.last_now = now;
        let mut views = Vec::new();
        let mut due = Vec::new();
        let mut events = Vec::new();

        // A pid present last poll but gone now has exited. `update` only runs
        // on a successful detection, so an unreachable WSL (which would make
        // every session "vanish" at once) never reaches this path.
        let live: Vec<i32> = snap.sessions.iter().map(|s| s.pid).collect();
        for s in &prev {
            if !live.contains(&s.pid) && self.tracked.contains_key(&s.pid) {
                events.push(SessionEvent {
                    pid: s.pid,
                    kind: EventKind::Exited,
                    project: basename(&s.cwd),
                });
            }
        }
        self.tracked.retain(|k, _| live.contains(k));

        for s in &snap.sessions {
            let (state, label) = classify(s, &self.settings);
            let t = self.tracked.entry(s.pid).or_default();
            let prev_state = t.last_state;
            let prev_waiting = t.was_waiting_input;

            if t.last_state == Some(SessionState::Red) && state != SessionState::Red {
                events.push(SessionEvent {
                    pid: s.pid,
                    kind: EventKind::Recovered,
                    project: basename(&s.cwd),
                });
            }

            if state == SessionState::Red {
                if t.blocked_since.is_none() {
                    t.blocked_since = Some(now);
                    events.push(SessionEvent {
                        pid: s.pid,
                        kind: EventKind::TurnedRed,
                        project: basename(&s.cwd),
                    });
                }
            } else if t.blocked_since.is_some() {
                // recovered or never was blocked — reset the episode
                *t = Tracked::default();
            }

            // claude finished its turn and now waits for the human — worth a
            // poke for anyone running long unattended jobs. Fire on the edge
            // only (and never on the very first sighting: a monitor started
            // mid-wait should not report a turn that ended hours ago).
            let waiting_input = state == SessionState::Yellow && label == "等待输入";
            if waiting_input && !prev_waiting && prev_state.is_some() {
                events.push(SessionEvent {
                    pid: s.pid,
                    kind: EventKind::TurnEnd,
                    project: basename(&s.cwd),
                });
            }
            t.was_waiting_input = waiting_input;
            t.last_state = Some(state);

            let controllable = s.tmux.is_some();
            let mut remaining: Option<i64> = None;
            // No countdown for a session we cannot control: showing one would
            // promise a key press that is never going to happen.
            if state == SessionState::Red
                && controllable
                && self.settings.auto_continue
                && t.sends < self.settings.max_sends
            {
                let next_at = if t.sends == 0 {
                    t.blocked_since.unwrap_or(now) + self.settings.wait_secs as i64
                } else {
                    t.last_send_at.unwrap_or(now) + self.settings.retry_interval_secs as i64
                };
                if next_at > now {
                    remaining = Some(next_at - now);
                } else {
                    // Count the attempt right here, while the engine lock is
                    // still held. Scheduling and bookkeeping have to be one
                    // step: two polls racing on the same snapshot would
                    // otherwise both see `sends == 0` and press Enter twice.
                    t.sends += 1;
                    t.last_send_at = Some(now);
                    due.push(s.pid);
                    if t.sends < self.settings.max_sends {
                        remaining = Some(self.settings.retry_interval_secs as i64);
                    }
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
                usage: s.usage,
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
        (views, due, events)
    }

    /// Mark that resume keys were sent to `pid` by hand. Automatic sends are
    /// counted by `update` itself.
    pub fn record_send(&mut self, pid: i32) {
        let t = self.tracked.entry(pid).or_default();
        t.sends += 1;
        // Before the first successful poll there is no clock to schedule
        // with. Leaving `last_send_at` unset makes the next poll schedule a
        // retry from its own "now" instead of from epoch 0 — which would
        // have fired a spurious immediate retry.
        if self.last_now > 0 {
            t.last_send_at = Some(self.last_now);
        }
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
            transcript: Some("/home/john/.claude/projects/x/abc.jsonl".into()),
            session_id: "abc".into(),
            last_type: last_type.into(),
            last_ts: String::new(),
            idle_sec: Some(idle),
            preview: "做点什么".into(),
            usage: None,
            tool_running: false,
            transcript_live: true,
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
        let (v, due, _) = e.update(snap(1000, vec![session(1, "assistant", 10)]));
        assert_eq!(v[0].state, SessionState::Green);
        assert!(due.is_empty());
    }

    /// Usage is display data: it must reach the view intact and never
    /// change the classification.
    #[test]
    fn usage_passes_through_to_view() {
        let mut e = Engine::new(Settings::default());
        let mut s = session(1, "assistant", 10);
        s.usage = Some(crate::detector::RawUsage {
            input: 1_100_000,
            cache_read: 1_200_000,
            cache_creation: 5_000,
            output: 89_000,
            requests: 42,
        });
        let (v, _, _) = e.update(snap(1000, vec![s]));
        assert_eq!(v[0].state, SessionState::Green);
        let u = v[0].usage.expect("usage present");
        assert_eq!((u.input, u.output, u.requests), (1_100_000, 89_000, 42));
    }

    #[test]
    fn yellow_waiting_for_input() {
        let mut e = Engine::new(Settings::default());
        let (v, _, _) = e.update(snap(1000, vec![session(1, "assistant", 300)]));
        assert_eq!(v[0].state, SessionState::Yellow);
        assert_eq!(v[0].label, "等待输入");
    }

    #[test]
    fn yellow_then_red_when_waiting_on_api() {
        let mut e = Engine::new(Settings::default());
        // default blocked_after_secs = 300
        let (v, _, _) = e.update(snap(1000, vec![session(1, "user", 240)]));
        assert_eq!(v[0].state, SessionState::Yellow);

        let (v, _, _) = e.update(snap(1300, vec![session(1, "user", 540)]));
        assert_eq!(v[0].state, SessionState::Red);
        assert_eq!(v[0].blocked_since, Some(1300));
    }

    #[test]
    fn auto_continue_fires_after_wait() {
        let mut e = Engine::new(Settings {
            wait_secs: 100,
            ..Default::default()
        });
        let t0 = 10_000;
        let (v, due, _) = e.update(snap(t0, vec![session(1, "user", 1000)]));
        assert_eq!(v[0].state, SessionState::Red);
        assert!(due.is_empty());
        assert_eq!(v[0].remaining_sec, Some(100));
        assert_eq!(v[0].sends, 0);

        let (v, due, _) = e.update(snap(t0 + 99, vec![session(1, "user", 1099)]));
        assert!(due.is_empty());
        assert_eq!(v[0].remaining_sec, Some(1));

        // the attempt is counted when it is scheduled, not by the caller
        let (v, due, _) = e.update(snap(t0 + 100, vec![session(1, "user", 1100)]));
        assert_eq!(due, vec![1]);
        assert_eq!(v[0].sends, 1);
        assert_eq!(v[0].last_send_at, Some(t0 + 100));
        // retry scheduled retry_interval_secs (600) after the send
        assert_eq!(v[0].remaining_sec, Some(600));
    }

    #[test]
    fn a_second_poll_of_the_same_snapshot_never_sends_twice() {
        let mut e = Engine::new(Settings {
            wait_secs: 0,
            ..Default::default()
        });
        let t0 = 10_000;
        let (_, due, _) = e.update(snap(t0, vec![session(1, "user", 5000)]));
        assert_eq!(due, vec![1]);

        // overlapping poll (slow WSL, a manual refresh) on an unchanged
        // snapshot: the key press must not be scheduled a second time
        let (v, due, _) = e.update(snap(t0, vec![session(1, "user", 5000)]));
        assert!(due.is_empty());
        assert_eq!(v[0].sends, 1);
    }

    #[test]
    fn max_sends_cap() {
        let mut e = Engine::new(Settings {
            wait_secs: 0,
            retry_interval_secs: 0,
            max_sends: 2,
            ..Default::default()
        });
        let t0 = 10_000;
        for i in 0..5 {
            e.update(snap(t0 + i, vec![session(1, "user", 5000)]));
        }
        let (v, due, _) = e.update(snap(t0 + 10, vec![session(1, "user", 5010)]));
        assert_eq!(v[0].sends, 2);
        assert!(due.is_empty());
        assert_eq!(v[0].remaining_sec, None, "no retry once the cap is reached");
    }

    #[test]
    fn recovers_to_green_resets_episode() {
        let mut e = Engine::new(Settings::default());
        let t0 = 10_000;
        let (_, _, _) = e.update(snap(t0, vec![session(1, "user", 1000)]));
        let (v, _, _) = e.update(snap(t0 + 1, vec![session(1, "user", 1001)]));
        assert!(v[0].blocked_since.is_some());

        let (v, _, _) = e.update(snap(t0 + 2, vec![session(1, "assistant", 2)]));
        assert_eq!(v[0].state, SessionState::Green);
        assert!(v[0].blocked_since.is_none());

        // turns red again → fresh episode
        let (v, _, _) = e.update(snap(t0 + 3, vec![session(1, "user", 1000)]));
        assert_eq!(v[0].blocked_since, Some(t0 + 3));
    }

    #[test]
    fn not_controllable_without_tmux() {
        let mut s = session(1, "user", 1000);
        s.tmux = None;
        let mut e = Engine::new(Settings {
            wait_secs: 0,
            ..Default::default()
        });
        let t0 = 10_000;
        let (v, due, _) = e.update(snap(t0, vec![s]));
        assert_eq!(v[0].state, SessionState::Red);
        assert!(!v[0].controllable);
        assert!(due.is_empty()); // never auto-sends to an uncontrolled session
        assert_eq!(v[0].remaining_sec, None, "no phantom countdown");
    }

    /// A process with no transcript yet (or one whose mapping we could not
    /// establish) must never be treated as stuck — that is what would press
    /// Enter in a terminal we know nothing about.
    #[test]
    fn no_usable_transcript_is_never_red() {
        let mut e = Engine::new(Settings {
            wait_secs: 0,
            ..Default::default()
        });

        let mut missing = session(1, "user", 99_999);
        missing.transcript = None;
        missing.transcript_live = false;

        // a fresh process handed an old session file by the fallback pairing
        let mut borrowed = session(2, "user", 99_999);
        borrowed.transcript_live = false;

        let (v, due, _) = e.update(snap(10_000, vec![missing, borrowed]));
        assert!(due.is_empty());
        for view in &v {
            assert_eq!(view.state, SessionState::Yellow);
            assert!(view.blocked_since.is_none());
        }
        let label = |pid: i32| v.iter().find(|s| s.pid == pid).unwrap().label.clone();
        assert_eq!(label(1), "未找到记录");
        assert_eq!(label(2), "记录未就绪");
    }

    #[test]
    fn tool_running_never_red() {
        // a long-running tool (assistant entry whose last block is tool_use)
        // must not be classified as blocked, no matter how long it runs
        let mut e = Engine::new(Settings {
            wait_secs: 0,
            ..Default::default()
        });
        let mut s = session(1, "assistant", 7200); // 2h "idle" while tool runs
        s.tool_running = true;
        let t0 = 10_000;
        let (v, due, _) = e.update(snap(t0, vec![s]));
        assert_eq!(v[0].state, SessionState::Yellow);
        assert_eq!(v[0].label, "工具运行中");
        assert!(due.is_empty());
        assert!(v[0].blocked_since.is_none());
    }

    #[test]
    fn sorts_red_first() {
        let mut e = Engine::new(Settings::default());
        let (v, _, _) = e.update(snap(
            1000,
            vec![session(5, "assistant", 10), session(9, "user", 900)],
        ));
        assert_eq!(v[0].pid, 9);
        assert_eq!(v[1].pid, 5);
    }

    #[test]
    fn turn_end_fires_once_per_wait() {
        let mut e = Engine::new(Settings::default());
        let t0 = 10_000;
        // first sight is already waiting → no event: a monitor started
        // mid-wait must not report a turn that ended hours ago
        let (_, _, evs) = e.update(snap(t0, vec![session(1, "assistant", 3000)]));
        assert!(evs.is_empty(), "{evs:?}");

        // actively working
        let (_, _, evs) = e.update(snap(t0 + 10, vec![session(1, "assistant", 2)]));
        assert!(evs.is_empty(), "{evs:?}");

        // turn ends, claude waits for the human → exactly one event
        let (_, _, evs) = e.update(snap(t0 + 600, vec![session(1, "assistant", 300)]));
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::TurnEnd);
        assert_eq!(evs[0].pid, 1);
        assert_eq!(evs[0].project, "statebar");

        // still waiting on the next poll → no repeat
        let (_, _, evs) = e.update(snap(t0 + 610, vec![session(1, "assistant", 310)]));
        assert!(evs.is_empty(), "{evs:?}");
    }

    #[test]
    fn red_and_recovery_events() {
        let mut e = Engine::new(Settings::default());
        let t0 = 10_000;
        let (_, _, evs) = e.update(snap(t0, vec![session(1, "assistant", 2)]));
        assert!(evs.is_empty(), "nothing happens on a healthy first poll");

        // goes red
        let (_, _, evs) = e.update(snap(t0 + 400, vec![session(1, "user", 400)]));
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::TurnedRed);
        assert_eq!(evs[0].project, "statebar");

        // stays red — quiet
        let (_, _, evs) = e.update(snap(t0 + 500, vec![session(1, "user", 500)]));
        assert!(evs.is_empty(), "{evs:?}");

        // recovers on its own
        let (_, _, evs) = e.update(snap(t0 + 600, vec![session(1, "assistant", 1)]));
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::Recovered);
    }

    #[test]
    fn exit_event_when_a_process_disappears() {
        let mut e = Engine::new(Settings::default());
        let t0 = 10_000;
        let (_, _, evs) = e.update(snap(
            t0,
            vec![session(1, "assistant", 10), session(2, "assistant", 10)],
        ));
        assert!(evs.is_empty());

        // pid 2 is gone
        let (_, _, evs) = e.update(snap(t0 + 5, vec![session(1, "assistant", 10)]));
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::Exited);
        assert_eq!(evs[0].pid, 2);

        // and a monitor that starts with one process reports nothing as exited
        let mut e2 = Engine::new(Settings::default());
        let (_, _, evs) = e2.update(snap(t0, vec![session(1, "assistant", 10)]));
        assert!(evs.is_empty(), "{evs:?}");
    }

    #[test]
    fn manual_send_before_first_poll_does_not_schedule_from_epoch_zero() {
        let mut e = Engine::new(Settings::default());
        // the user clicks "continue now" before any poll has ever succeeded
        e.record_send(1);
        let (v, due, _) = e.update(snap(10_000, vec![session(1, "user", 5000)]));
        assert!(
            due.is_empty(),
            "retry must be scheduled from now, not epoch 0"
        );
        assert_eq!(v[0].sends, 1);
        assert_eq!(v[0].last_send_at, None);

        // once a clock exists, a manual send is scheduled from it
        e.record_send(1);
        let (v, _, _) = e.update(snap(10_001, vec![session(1, "user", 5000)]));
        assert_eq!(v[0].sends, 2);
        assert_eq!(v[0].last_send_at, Some(10_000));
    }
}
