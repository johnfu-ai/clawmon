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

/// Why a session is in its state — the machine-readable half of `classify`.
/// The webview renders it through ui/i18n.js `reason.*` entries; core keeps
/// no display text, so the vocabulary can never drift from the state
/// machine that produces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Active,
    NoTranscript,
    TranscriptStale,
    /// a tool_use block is the last transcript activity and its result is
    /// still pending — claude is waiting on the tool, not on the user
    ToolRunning,
    /// claude is parked on a subagent (Task/Agent spawn or a blocking
    /// TaskOutput poll) — green, because the subagents are the ones
    /// making progress and the wait can outlast any idle window
    WaitingSubagent,
    /// the session is waiting for the human: the turn ended, or a parked
    /// AskUserQuestion whose "tool result" IS the user's answer
    WaitingInput,
    /// the last entry is a user prompt and claude has not answered yet —
    /// waiting on the API, which needs no user action however long it takes
    WaitingResponse,
    ResponseTimedOut,
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

/// What the countdown row under a red session shows. Computed here, where
/// the scheduling policy lives, so the webview can switch on the tag
/// instead of re-deriving the reason from correlated fields (which it used
/// to get wrong: a manual send with auto-continue off displayed "limit
/// reached" when no limit had been hit).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Countdown {
    /// auto-continue will fire in this many seconds
    #[serde(rename_all = "camelCase")]
    Waiting { remaining_sec: i64 },
    /// every configured attempt has been used up
    Capped { sends: u32 },
    /// auto-continue is disabled — the manual button still works
    Off,
    /// not inside tmux — nothing can be sent at all
    NoTmux,
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
    /// classification tag; see `Reason` — the display text lives in the UI
    pub reason: Reason,
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
    pub sends: u32,
    pub last_send_at: Option<i64>,
    /// present only while the session is red; see `Countdown`
    pub countdown: Option<Countdown>,
}

/// (red, yellow, green) counts plus a warning flag — the aggregate the tray
/// tooltip and the desktop pet render.
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusCounts {
    pub red: u32,
    pub yellow: u32,
    pub green: u32,
    pub warning: bool,
}

pub fn state_counts(sessions: &[SessionView]) -> StatusCounts {
    let mut c = StatusCounts::default();
    for s in sessions {
        match s.state {
            SessionState::Red => c.red += 1,
            SessionState::Yellow => c.yellow += 1,
            SessionState::Green => c.green += 1,
        }
    }
    c
}

pub struct Engine {
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

/// Light semantics: green means "no user action needed" — claude is working,
/// waiting on the API, or waiting on a tool/subagent. Yellow means the session
/// is waiting for the human, or we cannot tell what it is waiting for.
/// Red stays exclusively the auto-continue case: a user prompt that has gone
/// unanswered past the timeout.
fn classify(s: &RawSession, st: &Settings) -> (SessionState, Reason) {
    let idle = s.idle_sec.unwrap_or(i64::MAX);
    // transcript activity within the green window → actively working
    if idle < st.idle_green_secs {
        return (SessionState::Green, Reason::Active);
    }
    // Without a transcript we can vouch for, "idle for six hours" means
    // nothing: either claude has not written anything yet, or the file
    // belongs to a different session. Never call that blocked — the cost of a
    // false positive is pressing Enter in an innocent terminal — and never
    // call it healthy either: it may well be parked on the user, so it stays
    // yellow with every other "cannot tell" case.
    if s.transcript.is_none() {
        return (SessionState::Yellow, Reason::NoTranscript);
    }
    if !s.transcript_live {
        return (SessionState::Yellow, Reason::TranscriptStale);
    }
    // a tool_use block is the last transcript activity: the tool result is
    // only appended when the tool *finishes*, so claude is waiting on
    // something that is not the user — green for a running tool, and green
    // for a subagent wait too: the subagents write their own transcripts
    // while this one goes quiet, the wait routinely outlasts every idle
    // window, and it is forward progress. The one tool whose result IS a
    // user action is AskUserQuestion — that park is a genuine wait for input.
    if s.tool_running {
        if s.tool_name == "AskUserQuestion" {
            return (SessionState::Yellow, Reason::WaitingInput);
        }
        if matches!(s.tool_name.as_str(), "Task" | "Agent" | "TaskOutput") {
            return (SessionState::Green, Reason::WaitingSubagent);
        }
        return (SessionState::Green, Reason::ToolRunning);
    }
    // claude finished its turn and is waiting for the human
    if s.last_type == "assistant" {
        return (SessionState::Yellow, Reason::WaitingInput);
    }
    // only a trailing *user* prompt is "waiting on the API" — slow is not
    // stuck, so green until the timeout window says otherwise. A `system`
    // record after a finished reply (`turn_duration`, hook summary) is
    // bookkeeping — treating it as a hang would go red and press Enter.
    if s.last_type == "user" {
        if idle >= st.blocked_after_secs {
            (SessionState::Red, Reason::ResponseTimedOut)
        } else {
            (SessionState::Green, Reason::WaitingResponse)
        }
    } else {
        (SessionState::Yellow, Reason::WaitingInput)
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
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
    /// the caller only has to send the keys — it must not book the send
    /// again for these pids.
    pub fn update(
        &mut self,
        snap: RawStatus,
        settings: &Settings,
    ) -> (Vec<SessionView>, Vec<i32>, Vec<SessionEvent>) {
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
            let (state, reason) = classify(s, settings);
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
            let waiting_input = state == SessionState::Yellow && reason == Reason::WaitingInput;
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
            // No countdown for a session we cannot control: showing one would
            // promise a key press that is never going to happen.
            let mut countdown = None;
            if state == SessionState::Red {
                countdown = Some(if !controllable {
                    Countdown::NoTmux
                } else if !settings.auto_continue {
                    Countdown::Off
                } else if t.sends >= settings.max_sends {
                    Countdown::Capped { sends: t.sends }
                } else {
                    let next_at = if t.sends == 0 {
                        t.blocked_since.unwrap_or(now) + settings.wait_secs as i64
                    } else {
                        t.last_send_at.unwrap_or(now) + settings.retry_interval_secs as i64
                    };
                    if next_at > now {
                        Countdown::Waiting {
                            remaining_sec: next_at - now,
                        }
                    } else {
                        // Count the attempt right here, while the engine lock
                        // is still held. Scheduling and bookkeeping have to be
                        // one step: two polls racing on the same snapshot
                        // would otherwise both see `sends == 0` and press
                        // Enter twice.
                        t.sends += 1;
                        t.last_send_at = Some(now);
                        due.push(s.pid);
                        if t.sends < settings.max_sends {
                            Countdown::Waiting {
                                remaining_sec: settings.retry_interval_secs as i64,
                            }
                        } else {
                            Countdown::Capped { sends: t.sends }
                        }
                    }
                });
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
                reason,
                idle_sec: s.idle_sec,
                preview: s.preview.clone(),
                usage: s.usage,
                pane: s.tmux.as_ref().map(|t| t.pane.clone()),
                controllable,
                blocked_since: t.blocked_since,
                sends: t.sends,
                last_send_at: t.last_send_at,
                countdown,
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

    /// Book a manual send attempt BEFORE the keys go out. The auto path
    /// books inside `update` under the same lock; booking first here extends
    /// that invariant to manual sends — a poll that fires during the
    /// blocking WSL round trip sees `last_send_at` and schedules its retry
    /// from there instead of pressing Enter a second time.
    pub fn claim_send(&mut self, pid: i32) {
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

impl Default for Engine {
    fn default() -> Self {
        Self::new()
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
            tool_name: String::new(),
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
        let st = Settings::default();
        let mut e = Engine::new();
        let (v, due, _) = e.update(snap(1000, vec![session(1, "assistant", 10)]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::Active);
        assert!(due.is_empty());
    }

    /// Usage is display data: it must reach the view intact and never
    /// change the classification.
    #[test]
    fn usage_passes_through_to_view() {
        let st = Settings::default();
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 10);
        s.usage = Some(crate::detector::RawUsage {
            input: 1_100_000,
            cache_read: 1_200_000,
            cache_creation: 5_000,
            output: 89_000,
            requests: 42,
        });
        let (v, _, _) = e.update(snap(1000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        let u = v[0].usage.expect("usage present");
        assert_eq!((u.input, u.output, u.requests), (1_100_000, 89_000, 42));
    }

    /// The webview reads `SessionView` verbatim (ui/app.js renders every
    /// field; the usage numbers at app.js:88-94, the countdown at :98-111).
    /// This pins the serialized key set so a Rust-side rename cannot drift
    /// past CI — any change here must be cross-checked against those readers.
    #[test]
    fn session_view_serializes_the_wire_contract() {
        let st = Settings {
            wait_secs: 100,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut s = session(1, "user", 1000);
        s.usage = Some(crate::detector::RawUsage {
            input: 1,
            cache_read: 2,
            cache_creation: 3,
            output: 4,
            requests: 5,
        });
        let (v, _, _) = e.update(snap(10_000, vec![s]), &st);
        let obj = serde_json::to_value(&v[0]).unwrap();
        let mut keys: Vec<&str> = obj
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys.join(","),
            "blockedSince,controllable,countdown,cwd,idleSec,lastSendAt,pane,pid,\
             preview,project,reason,sends,sessionId,state,tmuxLabel,tty,usage"
        );
        assert_eq!(obj["reason"], "response_timed_out");
        let usage = &obj["usage"];
        let mut uk: Vec<&str> = usage
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        uk.sort_unstable();
        // camelCase for the webview (ui/app.js:91 reads cacheRead/cacheCreation)
        assert_eq!(
            uk.join(","),
            "cacheCreation,cacheRead,input,output,requests"
        );
        let cd = &obj["countdown"];
        assert_eq!(cd["kind"], "waiting");
        assert_eq!(cd["remainingSec"], 100);
    }

    #[test]
    fn yellow_waiting_for_input() {
        let st = Settings::default();
        let mut e = Engine::new();
        let (v, _, _) = e.update(snap(1000, vec![session(1, "assistant", 300)]), &st);
        assert_eq!(v[0].state, SessionState::Yellow);
        assert_eq!(v[0].reason, Reason::WaitingInput);
    }

    /// A finished turn is often followed by timestamped `system` records
    /// (`turn_duration`, hook summaries). Those must not look like a hang
    /// waiting on the API — false red auto-sends Enter.
    #[test]
    fn system_trailer_after_a_turn_is_waiting_input_not_red() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let (v, due, _) = e.update(snap(10_000, vec![session(1, "system", 1000)]), &st);
        assert_eq!(v[0].state, SessionState::Yellow);
        assert_eq!(v[0].reason, Reason::WaitingInput);
        assert!(due.is_empty());
        assert!(v[0].blocked_since.is_none());
    }

    #[test]
    fn green_then_red_when_waiting_on_api() {
        let st = Settings::default();
        let mut e = Engine::new();
        // default blocked_after_secs = 300; waiting on the API needs no user
        // action, however long it takes — green while merely slow
        let (v, _, _) = e.update(snap(1000, vec![session(1, "user", 240)]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::WaitingResponse);

        let (v, _, _) = e.update(snap(1300, vec![session(1, "user", 540)]), &st);
        assert_eq!(v[0].state, SessionState::Red);
        assert_eq!(v[0].reason, Reason::ResponseTimedOut);
        assert_eq!(v[0].blocked_since, Some(1300));
    }

    #[test]
    fn auto_continue_fires_after_wait() {
        let st = Settings {
            wait_secs: 100,
            ..Default::default()
        };
        let mut e = Engine::new();
        let t0 = 10_000;
        let (v, due, _) = e.update(snap(t0, vec![session(1, "user", 1000)]), &st);
        assert_eq!(v[0].state, SessionState::Red);
        assert!(due.is_empty());
        assert_eq!(waiting_remaining(&v[0]), Some(100));
        assert_eq!(v[0].sends, 0);

        let (v, due, _) = e.update(snap(t0 + 99, vec![session(1, "user", 1099)]), &st);
        assert!(due.is_empty());
        assert_eq!(waiting_remaining(&v[0]), Some(1));

        // the attempt is counted when it is scheduled, not by the caller
        let (v, due, _) = e.update(snap(t0 + 100, vec![session(1, "user", 1100)]), &st);
        assert_eq!(due, vec![1]);
        assert_eq!(v[0].sends, 1);
        assert_eq!(v[0].last_send_at, Some(t0 + 100));
        // retry scheduled retry_interval_secs (600) after the send
        assert_eq!(waiting_remaining(&v[0]), Some(600));
    }

    fn waiting_remaining(v: &SessionView) -> Option<i64> {
        match v.countdown {
            Some(Countdown::Waiting { remaining_sec }) => Some(remaining_sec),
            _ => None,
        }
    }

    #[test]
    fn a_second_poll_of_the_same_snapshot_never_sends_twice() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let t0 = 10_000;
        let (_, due, _) = e.update(snap(t0, vec![session(1, "user", 5000)]), &st);
        assert_eq!(due, vec![1]);

        // overlapping poll (slow WSL, a manual refresh) on an unchanged
        // snapshot: the key press must not be scheduled a second time
        let (v, due, _) = e.update(snap(t0, vec![session(1, "user", 5000)]), &st);
        assert!(due.is_empty());
        assert_eq!(v[0].sends, 1);
    }

    #[test]
    fn max_sends_cap() {
        let st = Settings {
            wait_secs: 0,
            retry_interval_secs: 0,
            max_sends: 2,
            ..Default::default()
        };
        let mut e = Engine::new();
        let t0 = 10_000;
        for i in 0..5 {
            e.update(snap(t0 + i, vec![session(1, "user", 5000)]), &st);
        }
        let (v, due, _) = e.update(snap(t0 + 10, vec![session(1, "user", 5010)]), &st);
        assert_eq!(v[0].sends, 2);
        assert!(due.is_empty());
        assert_eq!(
            v[0].countdown,
            Some(Countdown::Capped { sends: 2 }),
            "no retry once the cap is reached"
        );
    }

    #[test]
    fn recovers_to_green_resets_episode() {
        let st = Settings::default();
        let mut e = Engine::new();
        let t0 = 10_000;
        let (_, _, _) = e.update(snap(t0, vec![session(1, "user", 1000)]), &st);
        let (v, _, _) = e.update(snap(t0 + 1, vec![session(1, "user", 1001)]), &st);
        assert!(v[0].blocked_since.is_some());

        let (v, _, _) = e.update(snap(t0 + 2, vec![session(1, "assistant", 2)]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert!(v[0].blocked_since.is_none());

        // turns red again → fresh episode
        let (v, _, _) = e.update(snap(t0 + 3, vec![session(1, "user", 1000)]), &st);
        assert_eq!(v[0].blocked_since, Some(t0 + 3));
    }

    #[test]
    fn not_controllable_without_tmux() {
        let mut s = session(1, "user", 1000);
        s.tmux = None;
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let t0 = 10_000;
        let (v, due, _) = e.update(snap(t0, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Red);
        assert!(!v[0].controllable);
        assert!(due.is_empty()); // never auto-sends to an uncontrolled session
        assert_eq!(v[0].countdown, Some(Countdown::NoTmux));
    }

    /// A manual send with auto-continue off must say "off", not "limit
    /// reached" — the old payload let the webview infer a cap that was
    /// never hit.
    #[test]
    fn manual_send_with_auto_continue_off_shows_off() {
        let st = Settings {
            auto_continue: false,
            ..Default::default()
        };
        let mut e = Engine::new();
        let t0 = 10_000;
        let (v, _, _) = e.update(snap(t0, vec![session(1, "user", 1000)]), &st);
        assert_eq!(v[0].countdown, Some(Countdown::Off));
        e.claim_send(1);
        let (v, _, _) = e.update(snap(t0 + 1, vec![session(1, "user", 1001)]), &st);
        assert_eq!(v[0].sends, 1);
        assert_eq!(
            v[0].countdown,
            Some(Countdown::Off),
            "still off, not capped"
        );
    }

    /// A process with no transcript yet (or one whose mapping we could not
    /// establish) must never be treated as stuck — that is what would press
    /// Enter in a terminal we know nothing about.
    #[test]
    fn no_usable_transcript_is_never_red() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();

        let mut missing = session(1, "user", 99_999);
        missing.transcript = None;
        missing.transcript_live = false;

        // a fresh process handed an old session file by the fallback pairing
        let mut borrowed = session(2, "user", 99_999);
        borrowed.transcript_live = false;

        let (v, due, _) = e.update(snap(10_000, vec![missing, borrowed]), &st);
        assert!(due.is_empty());
        for view in &v {
            assert_eq!(view.state, SessionState::Yellow);
            assert!(view.blocked_since.is_none());
        }
        let reason = |pid: i32| v.iter().find(|s| s.pid == pid).unwrap().reason;
        assert_eq!(reason(1), Reason::NoTranscript);
        assert_eq!(reason(2), Reason::TranscriptStale);
    }

    #[test]
    fn tool_running_is_green() {
        // a long-running tool (assistant entry whose last block is tool_use)
        // waits on the tool, not on the user — green, and never blocked, no
        // matter how long it runs
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 7200); // 2h "idle" while tool runs
        s.tool_running = true;
        s.tool_name = "Bash".into();
        let t0 = 10_000;
        let (v, due, _) = e.update(snap(t0, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::ToolRunning);
        assert!(due.is_empty());
        assert!(v[0].blocked_since.is_none());
    }

    /// A parked AskUserQuestion is the one tool whose result IS a user
    /// action: waiting on it is waiting for input — yellow, the true
    /// user-interaction light, not green "tool running".
    #[test]
    fn parked_askuserquestion_waits_for_input() {
        let st = Settings::default();
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 7200);
        s.tool_running = true;
        s.tool_name = "AskUserQuestion".into();
        let (v, due, _) = e.update(snap(10_000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Yellow);
        assert_eq!(v[0].reason, Reason::WaitingInput);
        assert!(due.is_empty());
    }

    /// A session parked on "Waiting for task" — the pending tool is a
    /// subagent launch or a blocking TaskOutput poll. The subagents do the
    /// writing while the main transcript sits on its tool_use, so this is
    /// healthy progress however long it lasts: green, with its own tag,
    /// never yellow "waiting for input".
    #[test]
    fn waiting_on_a_subagent_is_green() {
        let st = Settings::default();
        let mut e = Engine::new();
        for name in ["Task", "Agent", "TaskOutput"] {
            let mut s = session(1, "assistant", 7200); // subagents ran 2h
            s.tool_running = true;
            s.tool_name = name.into();
            let (v, due, _) = e.update(snap(10_000, vec![s]), &st);
            assert_eq!(v[0].state, SessionState::Green, "{name}");
            assert_eq!(v[0].reason, Reason::WaitingSubagent, "{name}");
            assert!(due.is_empty());
            assert!(v[0].blocked_since.is_none());
        }
    }

    #[test]
    fn sorts_red_first() {
        let st = Settings::default();
        let mut e = Engine::new();
        let (v, _, _) = e.update(
            snap(
                1000,
                vec![session(5, "assistant", 10), session(9, "user", 900)],
            ),
            &st,
        );
        assert_eq!(v[0].pid, 9);
        assert_eq!(v[1].pid, 5);
    }

    #[test]
    fn state_counts_fold() {
        let st = Settings::default();
        let mut e = Engine::new();
        let (v, _, _) = e.update(
            snap(
                1000,
                vec![session(5, "assistant", 10), session(9, "user", 900)],
            ),
            &st,
        );
        let c = state_counts(&v);
        assert_eq!((c.red, c.yellow, c.green), (1, 0, 1));
        assert!(!c.warning);
    }

    #[test]
    fn turn_end_fires_once_per_wait() {
        let st = Settings::default();
        let mut e = Engine::new();
        let t0 = 10_000;
        // first sight is already waiting → no event: a monitor started
        // mid-wait must not report a turn that ended hours ago
        let (_, _, evs) = e.update(snap(t0, vec![session(1, "assistant", 3000)]), &st);
        assert!(evs.is_empty(), "{evs:?}");

        // actively working
        let (_, _, evs) = e.update(snap(t0 + 10, vec![session(1, "assistant", 2)]), &st);
        assert!(evs.is_empty(), "{evs:?}");

        // turn ends, claude waits for the human → exactly one event
        let (_, _, evs) = e.update(snap(t0 + 600, vec![session(1, "assistant", 300)]), &st);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::TurnEnd);
        assert_eq!(evs[0].pid, 1);
        assert_eq!(evs[0].project, "statebar");

        // still waiting on the next poll → no repeat
        let (_, _, evs) = e.update(snap(t0 + 610, vec![session(1, "assistant", 310)]), &st);
        assert!(evs.is_empty(), "{evs:?}");
    }

    #[test]
    fn red_and_recovery_events() {
        let st = Settings::default();
        let mut e = Engine::new();
        let t0 = 10_000;
        let (_, _, evs) = e.update(snap(t0, vec![session(1, "assistant", 2)]), &st);
        assert!(evs.is_empty(), "nothing happens on a healthy first poll");

        // goes red
        let (_, _, evs) = e.update(snap(t0 + 400, vec![session(1, "user", 400)]), &st);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::TurnedRed);
        assert_eq!(evs[0].project, "statebar");

        // stays red — quiet
        let (_, _, evs) = e.update(snap(t0 + 500, vec![session(1, "user", 500)]), &st);
        assert!(evs.is_empty(), "{evs:?}");

        // recovers on its own
        let (_, _, evs) = e.update(snap(t0 + 600, vec![session(1, "assistant", 1)]), &st);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::Recovered);
    }

    #[test]
    fn exit_event_when_a_process_disappears() {
        let st = Settings::default();
        let mut e = Engine::new();
        let t0 = 10_000;
        let (_, _, evs) = e.update(
            snap(
                t0,
                vec![session(1, "assistant", 10), session(2, "assistant", 10)],
            ),
            &st,
        );
        assert!(evs.is_empty());

        // pid 2 is gone
        let (_, _, evs) = e.update(snap(t0 + 5, vec![session(1, "assistant", 10)]), &st);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::Exited);
        assert_eq!(evs[0].pid, 2);

        // and a monitor that starts with one process reports nothing as exited
        let mut e2 = Engine::new();
        let (_, _, evs) = e2.update(snap(t0, vec![session(1, "assistant", 10)]), &st);
        assert!(evs.is_empty());
    }

    #[test]
    fn manual_send_before_first_poll_does_not_schedule_from_epoch_zero() {
        let st = Settings::default();
        let mut e = Engine::new();
        // the user clicks "continue now" before any poll has ever succeeded
        e.claim_send(1);
        let (v, due, _) = e.update(snap(10_000, vec![session(1, "user", 5000)]), &st);
        assert!(
            due.is_empty(),
            "retry must be scheduled from now, not epoch 0"
        );
        assert_eq!(v[0].sends, 1);
        assert_eq!(v[0].last_send_at, None);

        // once a clock exists, a manual send is scheduled from it
        e.claim_send(1);
        let (v, _, _) = e.update(snap(10_001, vec![session(1, "user", 5000)]), &st);
        assert_eq!(v[0].sends, 2);
        assert_eq!(v[0].last_send_at, Some(10_000));
    }
}
