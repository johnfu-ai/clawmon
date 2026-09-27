use crate::detector::{RawSession, RawStatus};
use crate::settings::Settings;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Green,
    /// the turn completed and claude awaits the next instruction — done, not
    /// stuck and not asking anything
    Blue,
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
    /// the turn ended and claude awaits the next instruction — blue, the
    /// "completed" light: nothing is wrong, it is simply done
    TurnComplete,
    /// the session is blocked on the human *mid-flight*: a parked
    /// AskUserQuestion whose "tool result" IS the user's answer
    WaitingInput,
    /// claude is parked on ExitPlanMode — the plan is written and the
    /// user has to approve it before the turn can continue
    WaitingApproval,
    /// a /goal (or similar) paused and told the user to send a message
    GoalPaused,
    /// the last entry is a user prompt and claude has not answered yet —
    /// waiting on the API, which needs no user action however long it takes
    WaitingResponse,
    ResponseTimedOut,
    /// a 5-hour (or similar) usage-limit 429 paused the goal; auto-continue
    /// waits for the reset timestamp parsed from the error, then sends Enter
    UsageLimited,
    /// a non-quota API error ended the turn. Retryable ones are a stall
    /// (auto-continue may get past them); 400/401/403/404 are not
    ApiError,
    /// claude itself said it will retry ("retrying in 1 min") — green,
    /// it is already handling the failure
    ApiRetrying,
    /// the user interrupted the turn; claude is back at the prompt
    Interrupted,
    /// sitting at the prompt with nothing owed: a fresh session, a local
    /// slash command that already ran, or a prompt whose process is idle
    /// so it is not actually waiting on the API
    Ready,
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
    /// parked on something only the user can answer (a question, a plan
    /// approval, a paused goal)
    NeedsInput,
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
    /// the previous poll already saw a finished turn — TurnEnd fires once
    saw_turn_complete: bool,
    /// the previous poll already saw a mid-flight ask — NeedsInput fires once
    saw_needs_input: bool,
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

/// (red, yellow, blue, green) counts plus a warning flag — the aggregate the
/// tray tooltip and the desktop pet render.
#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusCounts {
    pub red: u32,
    pub yellow: u32,
    pub blue: u32,
    pub green: u32,
    pub warning: bool,
}

pub fn state_counts(sessions: &[SessionView]) -> StatusCounts {
    let mut c = StatusCounts::default();
    for s in sessions {
        match s.state {
            SessionState::Red => c.red += 1,
            SessionState::Yellow => c.yellow += 1,
            SessionState::Blue => c.blue += 1,
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

/// Seconds after the parsed usage-window reset before the first Enter.
/// The error's clock is second-precise; sending on the exact second still
/// 429s, and one poll interval is not a reliable buffer.
const USAGE_LIMIT_GRACE_SECS: i64 = 30;

/// utime+stime ticks/s below which the process is sitting still.
/// Measured 2026-09-27, Claude Code 2.1.280, CLK_TCK 100: idle at the
/// prompt is 0–2 (a 1 Hz status timer; a rare one-second burst near 11
/// averages under 4 across the 5 s poll). A turn that is streaming sits
/// at 10–25. Unknown (None) is not idle — one-shot mode has no sample,
/// and the classifier then trusts the transcript alone.
const PROCESS_IDLE_TICKS_PER_SEC: f64 = 5.0;

fn basename(p: &str) -> String {
    let p = p.trim_end_matches('/');
    if p.is_empty() {
        return "/".into();
    }
    p.rsplit('/').next().unwrap_or(p).to_string()
}

fn process_is_idle(s: &RawSession) -> bool {
    match s.cpu_ticks_per_sec {
        Some(rate) => rate < PROCESS_IDLE_TICKS_PER_SEC,
        None => false,
    }
}

/// A stall is a failure auto-continue may be able to get past: a prompt
/// that went unanswered, a usage-limit 429, or a retryable API error.
/// A non-retryable API error is not a stall — Enter will not fix it.
fn is_stall(reason: Reason, api_retryable: bool) -> bool {
    match reason {
        Reason::ResponseTimedOut | Reason::UsageLimited => true,
        Reason::ApiError => api_retryable,
        _ => false,
    }
}

/// Yellow reasons where the session is waiting on the human mid-flight,
/// as opposed to "we cannot tell" (no transcript / stale transcript).
fn needs_input(reason: Reason) -> bool {
    matches!(
        reason,
        Reason::WaitingInput | Reason::WaitingApproval | Reason::GoalPaused
    )
}

/// Light from the reason plus whether clawmon can still fix a stall.
///
/// Green is "no user action needed", and that includes a stall whose
/// auto-continue countdown is still running. Red is only "a person has to
/// fix this": the stall cannot be auto-continued (not in tmux, auto-continue
/// off, attempts used up), or the API error is not retryable. Blue is a
/// finished turn or an idle prompt. Yellow is a mid-flight ask, or a
/// transcript we cannot trust — never red, never auto-sent.
fn light(
    reason: Reason,
    retryable: bool,
    controllable: bool,
    settings: &Settings,
    sends: u32,
) -> SessionState {
    if is_stall(reason, retryable) {
        let can_fix = controllable && settings.auto_continue && sends < settings.max_sends;
        return if can_fix {
            SessionState::Green
        } else {
            SessionState::Red
        };
    }
    match reason {
        Reason::ApiError => SessionState::Red,
        Reason::Active
        | Reason::WaitingResponse
        | Reason::ToolRunning
        | Reason::WaitingSubagent
        | Reason::ApiRetrying => SessionState::Green,
        Reason::TurnComplete | Reason::Interrupted | Reason::Ready => SessionState::Blue,
        Reason::WaitingInput
        | Reason::WaitingApproval
        | Reason::GoalPaused
        | Reason::NoTranscript
        | Reason::TranscriptStale => SessionState::Yellow,
        // stalls are handled above; listed so the match stays exhaustive
        Reason::ResponseTimedOut | Reason::UsageLimited => SessionState::Red,
    }
}

/// Why the session is where it is. The color is `light`'s decision: a stall
/// reason is green while auto-continue can still fire and red when it cannot.
fn classify(s: &RawSession, st: &Settings) -> Reason {
    let idle = s.idle_sec.unwrap_or(i64::MAX);
    // Without a transcript we can vouch for, never call the session blocked —
    // the cost of a false positive is pressing Enter in an innocent terminal.
    // A process that is sitting still with no transcript yet is a fresh
    // session at the prompt (blue). Anything we cannot prove idle stays
    // yellow with every other "cannot tell" case.
    if s.transcript.is_none() {
        return if process_is_idle(s) {
            Reason::Ready
        } else {
            Reason::NoTranscript
        };
    }
    if !s.transcript_live {
        return Reason::TranscriptStale;
    }
    // A usage-quota 429 is a synthetic assistant record with a turn trailer,
    // so the finished-turn check below would paint it done. It is a stall.
    if s.usage_limited {
        return Reason::UsageLimited;
    }
    // A parked AskUserQuestion is the one wait whose result IS the user.
    // It must outrank the subagent freshness below: the model can park on a
    // question while background agents keep running.
    if s.tool_running && s.tool_name == "AskUserQuestion" {
        return Reason::WaitingInput;
    }
    // ExitPlanMode is the same shape: the tool result is the user's approval.
    if s.tool_running && s.tool_name == "ExitPlanMode" {
        return Reason::WaitingApproval;
    }
    // A paused goal asked the user to send a message. That outranks a
    // trailer and fresh subagents — the ask is the point.
    if s.system_notice == "goal_paused" {
        return Reason::GoalPaused;
    }
    // Background subagents and dynamic workflows still writing their own
    // transcripts: Claude Code writes the turn trailer (and may log a later
    // slash command such as /workflows) while it holds the turn open waiting
    // for them. A fresh write is proof the wait does not need the user, so
    // it outranks that local command.
    if let Some(sub_idle) = s.subagent_idle_sec {
        if sub_idle < st.idle_subagent_secs {
            return Reason::WaitingSubagent;
        }
    }
    // Claude said it will retry on its own. Green, even though the turn
    // record is an API error and a trailer may already have landed.
    if s.system_notice == "retrying" {
        return Reason::ApiRetrying;
    }
    // Still generating a thought. A trailer after it means the turn ended.
    if s.thinking && !s.turn_complete {
        return Reason::Active;
    }
    if s.api_error {
        return Reason::ApiError;
    }
    // Not prompts the model owes a reply to. Checked before the idle window
    // so a local command a few seconds ago is "ready", not "running".
    if s.user_kind == "interrupt" {
        return Reason::Interrupted;
    }
    if s.user_kind == "local_command" || s.user_kind == "meta" {
        return Reason::Ready;
    }
    // The turn trailer is the completion signal, and it lands within seconds.
    // A mid-flight assistant status line has no trailer and stays active.
    if s.last_type == "assistant" && !s.tool_running && s.turn_complete {
        return Reason::TurnComplete;
    }
    if idle < st.idle_green_secs {
        return Reason::Active;
    }
    // a tool_use block is the last transcript activity: the tool result is
    // only appended when the tool finishes, so claude is waiting on
    // something that is not the user. A parked AskUserQuestion was already
    // answered above.
    if s.tool_running {
        if matches!(s.tool_name.as_str(), "Task" | "Agent" | "TaskOutput") {
            return Reason::WaitingSubagent;
        }
        return Reason::ToolRunning;
    }
    // only a trailing user prompt or tool result is "waiting on the API".
    // Past the timeout, a process that is sitting still is not waiting on
    // anything — pressing Enter there is the false red this signal exists
    // to prevent. Unknown CPU (one-shot, first poll) trusts the transcript.
    if s.last_type == "user" {
        if idle >= st.blocked_after_secs {
            if process_is_idle(s) {
                Reason::Ready
            } else {
                Reason::ResponseTimedOut
            }
        } else {
            Reason::WaitingResponse
        }
    } else {
        // assistant without a trailer, or a bookkeeping last_type we could
        // not classify: past the active window this looks finished.
        Reason::TurnComplete
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

    /// The raw sessions of the last successful `update` — task views resolve
    /// their linked claude session from this.
    pub fn last_snapshot(&self) -> &[RawSession] {
        &self.last_snapshot
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
            let reason = classify(s, settings);
            let stall = is_stall(reason, s.api_error_retryable);
            let t = self.tracked.entry(s.pid).or_default();
            let prev_state = t.last_state;
            let saw_turn = t.saw_turn_complete;
            let saw_input = t.saw_needs_input;
            let had_episode = t.blocked_since.is_some();

            // The episode is the stall, not the red light: a countdown that
            // clawmon is still handling stays green, and the bookkeeping
            // (when it started, how many Enters) has to survive that.
            if stall {
                if t.blocked_since.is_none() {
                    t.blocked_since = Some(now);
                }
            } else if had_episode {
                events.push(SessionEvent {
                    pid: s.pid,
                    kind: EventKind::Recovered,
                    project: basename(&s.cwd),
                });
                t.blocked_since = None;
                t.sends = 0;
                t.last_send_at = None;
            }

            let controllable = s.tmux.is_some();
            // Countdown for every stall, green or red. No countdown for a
            // session we cannot control: showing one would promise a key
            // press that is never going to happen. A non-retryable API
            // error is not a stall, so it gets no countdown either.
            let mut countdown = None;
            if stall {
                countdown = Some(if !controllable {
                    Countdown::NoTmux
                } else if !settings.auto_continue {
                    Countdown::Off
                } else if t.sends >= settings.max_sends {
                    Countdown::Capped { sends: t.sends }
                } else {
                    let next_at = if t.sends == 0 {
                        match s.resume_at {
                            // the error told us when the window reopens —
                            // sitting through another full wait_secs (5h)
                            // would miss a reset that is 90 minutes away
                            Some(at) => at + USAGE_LIMIT_GRACE_SECS,
                            None => t.blocked_since.unwrap_or(now) + settings.wait_secs as i64,
                        }
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

            // After the send is booked, so the poll that uses the last
            // attempt is already red — there is no further auto-continue.
            let state = light(
                reason,
                s.api_error_retryable,
                controllable,
                settings,
                t.sends,
            );
            let was_red = prev_state == Some(SessionState::Red);
            if state == SessionState::Red && !was_red {
                events.push(SessionEvent {
                    pid: s.pid,
                    kind: EventKind::TurnedRed,
                    project: basename(&s.cwd),
                });
            } else if was_red && state != SessionState::Red && !had_episode {
                // a red that was not a stall (a non-retryable API error)
                // cleared on its own
                events.push(SessionEvent {
                    pid: s.pid,
                    kind: EventKind::Recovered,
                    project: basename(&s.cwd),
                });
            }

            // Fire on the edge only, and never on the very first sighting:
            // a monitor started mid-wait must not report a turn that ended
            // hours ago, or a question that has been on screen all day.
            if prev_state.is_some() {
                if reason == Reason::TurnComplete && !saw_turn {
                    events.push(SessionEvent {
                        pid: s.pid,
                        kind: EventKind::TurnEnd,
                        project: basename(&s.cwd),
                    });
                }
                if needs_input(reason) && !saw_input {
                    events.push(SessionEvent {
                        pid: s.pid,
                        kind: EventKind::NeedsInput,
                        project: basename(&s.cwd),
                    });
                }
            }
            t.saw_turn_complete = reason == Reason::TurnComplete;
            t.saw_needs_input = needs_input(reason);
            t.last_state = Some(state);

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
                SessionState::Blue => 2,
                SessionState::Green => 3,
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
            turn_complete: false,
            thinking: false,
            transcript_live: true,
            usage_limited: false,
            resume_at: None,
            subagent_idle_sec: None,
            user_kind: String::new(),
            api_error: false,
            api_error_retryable: false,
            system_notice: String::new(),
            cpu_ticks_per_sec: None,
        }
    }

    fn snap(now: i64, sessions: Vec<RawSession>) -> RawStatus {
        RawStatus {
            now: String::new(),
            now_epoch: now as f64,
            sessions,
            tmux_sessions: Vec::new(),
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

    /// A finished turn is "done", not "asking you": blue with its own tag,
    /// so the yellow light stays reserved for mid-flight user blocks.
    #[test]
    fn completed_turn_is_blue() {
        let st = Settings::default();
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 300);
        s.turn_complete = true;
        let (v, _, _) = e.update(snap(1000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Blue);
        assert_eq!(v[0].reason, Reason::TurnComplete);
    }

    /// The turn trailer is the completion signal: blue as soon as it lands,
    /// without waiting out the active window. A mid-flight assistant status
    /// line (no trailer) stays green — the model is often still thinking.
    #[test]
    fn turn_trailer_flips_blue_immediately_mid_turn_stays_green() {
        let st = Settings::default();
        let mut e = Engine::new();
        // mid-turn status line, idle past the old 30 s grace → still running
        let (v, _, _) = e.update(snap(1000, vec![session(1, "assistant", 45)]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::Active);
        // trailer present, even a few seconds after the last text → done
        let mut done = session(1, "assistant", 5);
        done.turn_complete = true;
        let (v, _, _) = e.update(snap(1045, vec![done]), &st);
        assert_eq!(v[0].state, SessionState::Blue);
        assert_eq!(v[0].reason, Reason::TurnComplete);
    }

    /// Thinking-only assistant records are the model still generating.
    /// They must stay green however long they last — the 30 s "finished
    /// turn" heuristic used to flip them yellow/blue while claude worked.
    #[test]
    fn thinking_stays_green_past_the_active_window() {
        let st = Settings::default();
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 7200);
        s.thinking = true;
        let (v, due, _) = e.update(snap(10_000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::Active);
        assert!(due.is_empty());
    }

    /// last_entry missed (empty last_type, no idle): the UI showed yellow
    /// "等待输入" while claude was still running. Past the active window
    /// this looks finished, not a mid-flight ask — blue, never yellow.
    #[test]
    fn unknown_last_type_is_turn_complete_not_waiting_input() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut s = session(1, "", 1000);
        s.idle_sec = None;
        let (v, due, _) = e.update(snap(10_000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Blue);
        assert_eq!(v[0].reason, Reason::TurnComplete);
        assert!(due.is_empty());
        assert!(v[0].blocked_since.is_none());
    }

    /// A finished turn is often followed by timestamped `system` records
    /// (`turn_duration`, hook summaries). Those must not look like a hang
    /// waiting on the API — false red auto-sends Enter — and must not look
    /// like a mid-flight ask either.
    #[test]
    fn system_trailer_after_a_turn_is_complete_not_red() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let (v, due, _) = e.update(snap(10_000, vec![session(1, "system", 1000)]), &st);
        assert_eq!(v[0].state, SessionState::Blue);
        assert_eq!(v[0].reason, Reason::TurnComplete);
        assert!(due.is_empty());
        assert!(v[0].blocked_since.is_none());
    }

    /// Waiting on the API needs no user action, however long it takes — green
    /// while merely slow. Past the timeout it is still green when the session
    /// is in tmux and auto-continue can fire: the countdown is clawmon's job,
    /// not the user's. The same timeout outside tmux is red.
    #[test]
    fn timeout_is_green_while_auto_continue_can_fire_and_red_otherwise() {
        let st = Settings::default();
        let mut e = Engine::new();
        // default blocked_after_secs = 300
        let (v, _, _) = e.update(snap(1000, vec![session(1, "user", 240)]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::WaitingResponse);

        let (v, _, evs) = e.update(snap(1300, vec![session(1, "user", 540)]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::ResponseTimedOut);
        assert!(waiting_remaining(&v[0]).is_some());
        assert_eq!(v[0].blocked_since, Some(1300));
        assert!(
            evs.iter().all(|e| e.kind != EventKind::TurnedRed),
            "a countdown clawmon is handling is not a red episode: {evs:?}"
        );

        let mut bare = session(2, "user", 540);
        bare.tmux = None;
        let mut e2 = Engine::new();
        let (v, due, evs) = e2.update(snap(1300, vec![bare]), &st);
        assert_eq!(v[0].state, SessionState::Red);
        assert_eq!(v[0].reason, Reason::ResponseTimedOut);
        assert_eq!(v[0].countdown, Some(Countdown::NoTmux));
        assert!(due.is_empty());
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::TurnedRed);
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
        assert_eq!(
            v[0].state,
            SessionState::Green,
            "the countdown itself needs no user"
        );
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

    /// Regression (observed live 2026-09-26, session "rag"): Claude Code
    /// writes the `turn_duration` trailer right after the assistant's
    /// "agents are running" status text while it holds the turn open for
    /// background agents — 12 of them kept writing their own transcripts
    /// for minutes while clawmon showed blue "回合完成" with the turn
    /// "complete". The agents run in-process, so the only visible progress
    /// is fresh writes under <stem>/subagents/: while those are fresh the
    /// session is parked on subagents, not on the user.
    #[test]
    fn turn_trailer_with_fresh_subagents_stays_green() {
        let st = Settings::default();
        let mut e = Engine::new();
        // the photographed state: trailer landed, 39 s "idle", agents fresh
        let mut s = session(1, "assistant", 39);
        s.turn_complete = true;
        s.subagent_idle_sec = Some(5);
        let (v, due, _) = e.update(snap(1000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::WaitingSubagent);
        assert!(due.is_empty());
        assert!(v[0].blocked_since.is_none());
    }

    /// Regression (observed live 2026-09-27, session "rag"): a dynamic
    /// workflow keeps writing under subagents/workflows/<run>/ after the
    /// turn trailer, and a later /workflows slash command is recorded as
    /// a local_command. That command alone is "待命". Fresh workflow
    /// writes are the same proof as a background subagent, so the slash
    /// command must not win while they are fresh.
    #[test]
    fn fresh_workflow_agents_outrank_a_later_local_command() {
        let st = Settings::default();
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 120);
        s.turn_complete = true;
        s.user_kind = "local_command".into();
        s.subagent_idle_sec = Some(5);
        let (v, due, _) = e.update(snap(1000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::WaitingSubagent);
        assert!(due.is_empty());
        assert!(v[0].blocked_since.is_none());
    }

    /// The freshness window is what keeps the green provable: once the
    /// subagent transcripts have gone quiet past it, the trailer is the
    /// truth again and the finished turn settles to blue.
    #[test]
    fn stale_subagent_transcripts_do_not_hold_green() {
        let st = Settings::default();
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 7200);
        s.turn_complete = true;
        s.subagent_idle_sec = Some(st.idle_subagent_secs + 60);
        let (v, _, _) = e.update(snap(1000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Blue);
        assert_eq!(v[0].reason, Reason::TurnComplete);
    }

    /// A parked AskUserQuestion is waiting on the user *now* — background
    /// agents still running must not mask the yellow mid-flight ask.
    #[test]
    fn parked_askuserquestion_wins_over_fresh_subagents() {
        let st = Settings::default();
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 45);
        s.tool_running = true;
        s.tool_name = "AskUserQuestion".into();
        s.subagent_idle_sec = Some(5);
        let (v, due, _) = e.update(snap(1000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Yellow);
        assert_eq!(v[0].reason, Reason::WaitingInput);
        assert!(due.is_empty());
    }

    #[test]
    fn sorts_red_yellow_blue_green() {
        let st = Settings::default();
        let mut e = Engine::new();
        let (v, _, _) = e.update(
            snap(
                1000,
                vec![
                    session(5, "assistant", 10),  // green (active)
                    session(6, "assistant", 300), // blue (turn complete)
                    {
                        let mut s = session(9, "user", 900); // red: timed out, not in tmux
                        s.tmux = None;
                        s
                    },
                ],
            ),
            &st,
        );
        assert_eq!(v.iter().map(|s| s.pid).collect::<Vec<_>>(), [9, 6, 5]);
    }

    #[test]
    fn state_counts_fold() {
        let st = Settings::default();
        let mut e = Engine::new();
        let (v, _, _) = e.update(
            snap(
                1000,
                vec![
                    session(5, "assistant", 10),  // green (active)
                    session(6, "assistant", 300), // blue (turn complete)
                    {
                        let mut s = session(9, "user", 900); // red: timed out, not in tmux
                        s.tmux = None;
                        s
                    },
                ],
            ),
            &st,
        );
        let c = state_counts(&v);
        assert_eq!((c.red, c.yellow, c.blue, c.green), (1, 0, 1, 1));
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

        // goes red — not in tmux, so auto-continue cannot press Enter
        let mut stuck = session(1, "user", 400);
        stuck.tmux = None;
        let (_, _, evs) = e.update(snap(t0 + 400, vec![stuck]), &st);
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::TurnedRed);
        assert_eq!(evs[0].project, "statebar");

        // stays red — quiet
        let mut stuck = session(1, "user", 500);
        stuck.tmux = None;
        let (_, _, evs) = e.update(snap(t0 + 500, vec![stuck]), &st);
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

    /// The 5-hour usage-limit 429 is a synthetic assistant record with a
    /// turn trailer — the same shape as a finished turn. It is not done.
    /// Inside tmux the countdown is green (clawmon will press Enter at the
    /// reset); that is not a "needs a person" notification.
    #[test]
    fn usage_limit_countdown_is_green_until_the_reset() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 300);
        s.turn_complete = true;
        s.usage_limited = true;
        s.resume_at = Some(11_000);
        s.preview = "API Error: Request rejected (429) · [1308][已达到 5 小时的使用上限。您的限额将在 2026-09-19 16:21:07 重置。]".into();
        let (v, due, evs) = e.update(snap(10_000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::UsageLimited);
        assert!(
            due.is_empty(),
            "must wait for the parsed reset, not fire because wait_secs=0"
        );
        assert_eq!(
            waiting_remaining(&v[0]),
            Some(11_000 + USAGE_LIMIT_GRACE_SECS - 10_000)
        );
        assert!(
            evs.iter().all(|e| e.kind != EventKind::TurnedRed),
            "{evs:?}"
        );
    }

    /// First auto-continue is scheduled at the reset timestamp (plus a
    /// short grace), not `wait_secs` after we noticed the session. A
    /// window that has 90 minutes left must not sit through another 5h.
    #[test]
    fn usage_limit_auto_continue_fires_after_parsed_reset() {
        let st = Settings {
            wait_secs: 5 * 3600,
            ..Default::default()
        };
        let mut e = Engine::new();
        let limited = |idle| {
            let mut s = session(1, "assistant", idle);
            s.turn_complete = true;
            s.usage_limited = true;
            s.resume_at = Some(10_000 + 90 * 60);
            s
        };
        let t0 = 10_000;
        let (v, due, _) = e.update(snap(t0, vec![limited(0)]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert!(due.is_empty());
        let remain = waiting_remaining(&v[0]).expect("countdown");
        assert_eq!(remain, 90 * 60 + USAGE_LIMIT_GRACE_SECS);

        // one second before the grace window — still waiting
        let fire_at = t0 + remain;
        let (v, due, _) = e.update(snap(fire_at - 1, vec![limited(remain - 1)]), &st);
        assert!(due.is_empty());
        assert_eq!(waiting_remaining(&v[0]), Some(1));

        let (v, due, _) = e.update(snap(fire_at, vec![limited(remain)]), &st);
        assert_eq!(due, vec![1]);
        assert_eq!(v[0].sends, 1);
    }

    /// No parseable reset time: fall back to the configured wait, same
    /// as a hanging user prompt. Green while auto-continue can fire, and
    /// not "turn complete".
    #[test]
    fn usage_limit_without_resume_at_uses_wait_secs() {
        let st = Settings {
            wait_secs: 100,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut s = session(1, "assistant", 10);
        s.turn_complete = true;
        s.usage_limited = true;
        let (v, due, _) = e.update(snap(10_000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::UsageLimited);
        assert!(due.is_empty());
        assert_eq!(waiting_remaining(&v[0]), Some(100));
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

    /// A prompt past the timeout whose process is sitting still is not
    /// waiting on the API. Measured idle is under 2 ticks/s; 5 is the line.
    /// An idle process is never auto-sent Enter.
    #[test]
    fn idle_process_past_the_timeout_is_ready_not_red() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut s = session(1, "user", 5000);
        s.user_kind = "prompt".into();
        s.cpu_ticks_per_sec = Some(1.5);
        let (v, due, _) = e.update(snap(10_000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Blue);
        assert_eq!(v[0].reason, Reason::Ready);
        assert!(due.is_empty());
        assert!(v[0].countdown.is_none());
        assert!(v[0].blocked_since.is_none());

        // still busy (streaming, or CPU unknown): the timeout stands
        let mut busy = session(1, "user", 5000);
        busy.cpu_ticks_per_sec = Some(12.0);
        let (v, _, _) = e.update(snap(10_001, vec![busy]), &st);
        assert_eq!(v[0].reason, Reason::ResponseTimedOut);
        assert_eq!(v[0].state, SessionState::Green);
    }

    /// No transcript yet, and the process is idle: a fresh session at the
    /// prompt. Without a CPU sample we still cannot tell, so it stays yellow
    /// and is never auto-sent.
    #[test]
    fn fresh_idle_session_with_no_transcript_is_ready() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut s = session(1, "", 0);
        s.transcript = None;
        s.transcript_live = false;
        s.idle_sec = None;
        s.cpu_ticks_per_sec = Some(0.4);
        let (v, due, _) = e.update(snap(1000, vec![s]), &st);
        assert_eq!(v[0].state, SessionState::Blue);
        assert_eq!(v[0].reason, Reason::Ready);
        assert!(due.is_empty());
    }

    #[test]
    fn interrupt_and_local_command_are_ready_not_waiting_on_the_api() {
        let st = Settings {
            wait_secs: 0,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut interrupted = session(1, "user", 5000);
        interrupted.user_kind = "interrupt".into();
        let mut local = session(2, "user", 5000);
        local.user_kind = "local_command".into();
        let (v, due, _) = e.update(snap(1000, vec![interrupted, local]), &st);
        assert!(due.is_empty());
        let by = |pid: i32| v.iter().find(|s| s.pid == pid).unwrap();
        assert_eq!(by(1).state, SessionState::Blue);
        assert_eq!(by(1).reason, Reason::Interrupted);
        assert_eq!(by(2).state, SessionState::Blue);
        assert_eq!(by(2).reason, Reason::Ready);
    }

    /// A retryable API error is a stall: green with a countdown inside tmux,
    /// red when nothing can press Enter. A 400 is red either way, and Enter
    /// is never scheduled for it.
    #[test]
    fn retryable_api_error_counts_down_fatal_api_error_is_red() {
        let st = Settings {
            wait_secs: 100,
            ..Default::default()
        };
        let mut e = Engine::new();
        let mut dropped = session(1, "assistant", 30);
        dropped.turn_complete = true;
        dropped.api_error = true;
        dropped.api_error_retryable = true;
        let (v, due, evs) = e.update(snap(1000, vec![dropped]), &st);
        assert_eq!(v[0].state, SessionState::Green);
        assert_eq!(v[0].reason, Reason::ApiError);
        assert_eq!(waiting_remaining(&v[0]), Some(100));
        assert!(due.is_empty());
        assert!(evs.is_empty(), "{evs:?}");

        let mut bad = session(2, "assistant", 30);
        bad.turn_complete = true;
        bad.api_error = true;
        bad.api_error_retryable = false;
        bad.tmux = None;
        let mut e2 = Engine::new();
        let (v, due, evs) = e2.update(snap(1000, vec![bad]), &st);
        assert_eq!(v[0].state, SessionState::Red);
        assert_eq!(v[0].reason, Reason::ApiError);
        assert!(v[0].countdown.is_none(), "Enter cannot fix a 400");
        assert!(due.is_empty());
        assert_eq!(evs[0].kind, EventKind::TurnedRed);
    }

    #[test]
    fn goal_notices_and_plan_approval() {
        let st = Settings::default();
        let mut e = Engine::new();
        let mut retrying = session(1, "assistant", 40);
        retrying.api_error = true;
        retrying.api_error_retryable = true;
        retrying.system_notice = "retrying".into();
        let mut paused = session(2, "assistant", 40);
        paused.turn_complete = true;
        paused.system_notice = "goal_paused".into();
        let mut plan = session(3, "assistant", 40);
        plan.tool_running = true;
        plan.tool_name = "ExitPlanMode".into();
        // first sight of an ask does not notify
        let (v, due, evs) = e.update(snap(1000, vec![retrying, paused, plan]), &st);
        assert!(due.is_empty());
        assert!(evs.is_empty(), "{evs:?}");
        let by = |pid: i32| v.iter().find(|s| s.pid == pid).unwrap();
        assert_eq!(
            (by(1).state, by(1).reason),
            (SessionState::Green, Reason::ApiRetrying)
        );
        assert_eq!(
            (by(2).state, by(2).reason),
            (SessionState::Yellow, Reason::GoalPaused)
        );
        assert_eq!(
            (by(3).state, by(3).reason),
            (SessionState::Yellow, Reason::WaitingApproval)
        );

        // leaving the ask and coming back notifies once. Keep every pid, or
        // the ones that vanished would report Exited.
        let (_, _, evs) = e.update(
            snap(
                1010,
                vec![
                    session(1, "assistant", 2),
                    session(2, "assistant", 2),
                    session(3, "assistant", 2),
                ],
            ),
            &st,
        );
        assert!(evs.is_empty(), "{evs:?}");
        let mut paused = session(2, "assistant", 50);
        paused.system_notice = "goal_paused".into();
        let (_, _, evs) = e.update(
            snap(
                1020,
                vec![
                    session(1, "assistant", 2),
                    paused,
                    session(3, "assistant", 2),
                ],
            ),
            &st,
        );
        assert_eq!(evs.len(), 1, "{evs:?}");
        assert_eq!(evs[0].kind, EventKind::NeedsInput);
        assert_eq!(evs[0].pid, 2);
    }
}
