//! Task list (PRD FR10): user-defined "cd into a directory and run this"
//! one-click launches. The store owns persistence (`tasks.json`, same
//! discipline as settings.json) and the runtime status reconciliation
//! against the tmux session list each poll brings back.

use crate::detector::RawSession;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// How long a task may sit in `launching` without its tmux session ever
/// showing up before we call the launch failed (the session died instantly,
/// e.g. a bad directory or command). Generous on purpose: it must survive a
/// couple of slow polls, and a command that legitimately finished within the
/// window simply skips straight past "running".
pub const LAUNCH_GRACE_SECS: i64 = 30;

const MAX_TITLE: usize = 64;
const MAX_CWD: usize = 256;
const MAX_COMMAND: usize = 512;

/// A user-defined task. Only the definitions are persisted; runtime status
/// is recomputed from the tmux session list every poll.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Task {
    pub id: u64,
    pub title: String,
    /// WSL working directory, absolute (`/…` or `~/…`)
    pub cwd: String,
    /// shell command tmux runs in that directory
    pub command: String,
    /// epoch seconds
    pub created_at: i64,
    pub last_run_at: Option<i64>,
}

impl Task {
    /// The tmux session a launch of this task owns. App-generated, so it can
    /// never look like a tmux option or collide with a user's own session.
    pub fn session_name(&self) -> String {
        format!("clawmon-task-{}", self.id)
    }

    /// Reject rather than repair: `add`/`update` are user actions with an
    /// immediate error surface, unlike a hand-edited file (see `sanitize`).
    pub fn validate(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("标题不能为空".into());
        }
        if self.cwd.trim().is_empty() || !valid_path(&self.cwd) {
            return Err("目录必须是 WSL 绝对路径（/ 或 ~ 开头）".into());
        }
        if self.command.trim().is_empty() {
            return Err("命令不能为空".into());
        }
        Ok(())
    }

    /// Clamp in place: trim, cap lengths, drop anything invalid (a
    /// hand-edited tasks.json must not take the launch path down).
    pub fn sanitize(mut self) -> Option<Task> {
        self.title = truncate(self.title.trim(), MAX_TITLE);
        self.cwd = self.cwd.trim().to_string();
        self.command = truncate(self.command.trim(), MAX_COMMAND);
        self.validate().ok()?;
        Some(self)
    }
}

/// `~` and `/` are the only sane roots; no control characters, no newline
/// smuggling into the tmux command line.
fn valid_path(p: &str) -> bool {
    (p.starts_with('/') || p.starts_with('~'))
        && p.len() <= MAX_CWD
        && !p.chars().any(char::is_control)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    /// never run, or the last run ended
    Idle,
    /// new-session sent, waiting for the next poll to confirm
    Launching,
    /// the task's tmux session is alive
    Running,
    /// the tmux session is gone (command exited, incl. instant failure)
    Finished,
}

/// What the webview renders (PRD §7.2). Key set pinned by
/// `task_view_serializes_the_wire_contract` — extend that test together
/// with any field change, mirroring the SessionView discipline (D10).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: u64,
    pub title: String,
    pub cwd: String,
    pub command: String,
    pub status: TaskStatus,
    /// `clawmon-task-<id>` — what to attach a terminal to
    pub session_name: String,
    /// the claude session running inside the task's tmux session, when any
    pub session_id: Option<String>,
    pub pid: Option<i32>,
    pub created_at: i64,
    pub last_run_at: Option<i64>,
}

#[derive(Debug, Clone)]
struct Runtime {
    status: TaskStatus,
    /// epoch seconds of the transition into the current status
    since: i64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Persisted {
    next_id: u64,
    tasks: Vec<Task>,
}

impl Default for Persisted {
    fn default() -> Self {
        Self {
            next_id: 1,
            tasks: Vec::new(),
        }
    }
}

pub struct TaskStore {
    next_id: u64,
    tasks: Vec<Task>,
    runtime: HashMap<u64, Runtime>,
    path: PathBuf,
}

impl TaskStore {
    pub fn load(path: &Path) -> TaskStore {
        let persisted = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Persisted>(&s).ok())
            .unwrap_or_default();
        // drop hand-edited garbage; ids stay unique because next_id only grows
        let tasks: Vec<Task> = persisted
            .tasks
            .into_iter()
            .filter_map(|t| t.sanitize())
            .collect();
        TaskStore {
            next_id: persisted.next_id.max(1),
            tasks,
            runtime: HashMap::new(),
            path: path.to_path_buf(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let data = Persisted {
            next_id: self.next_id,
            tasks: self.tasks.clone(),
        };
        let json = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?;
        std::fs::write(&self.path, json).map_err(|e| e.to_string())
    }

    pub fn tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn status_of(&self, id: u64) -> TaskStatus {
        self.runtime
            .get(&id)
            .map(|r| r.status)
            .unwrap_or(TaskStatus::Idle)
    }

    pub fn add(&mut self, mut task: Task, now: i64) -> Result<Task, String> {
        task.id = 0; // ids are ours, never the caller's
        task.created_at = now;
        let task = task
            .sanitize()
            .ok_or_else(|| "任务不合法：标题/目录/命令不能为空，目录必须是绝对路径".to_string())?;
        task.validate()?;
        let id = self.next_id;
        self.next_id += 1;
        let task = Task { id, ..task };
        self.tasks.push(task.clone());
        Ok(task)
    }

    pub fn update(&mut self, id: u64, mut task: Task) -> Result<(), String> {
        task.id = id; // identity is the target of the edit, not a payload field
        let task = task
            .sanitize()
            .ok_or_else(|| "任务不合法：标题/目录/命令不能为空，目录必须是绝对路径".to_string())?;
        task.validate()?;
        let slot = self
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| format!("任务 {id} 不存在"))?;
        *slot = task;
        Ok(())
    }

    /// Remove a task definition. A running task's tmux session is left
    /// alone — deleting the row is not stopping the work.
    pub fn remove(&mut self, id: u64) -> Result<(), String> {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        self.runtime.remove(&id);
        if self.tasks.len() == before {
            return Err(format!("任务 {id} 不存在"));
        }
        Ok(())
    }

    /// Book a launch under the store lock BEFORE the WSL round trip (the
    /// same claim-then-execute invariant as `Engine::claim_send`): a second
    /// click, or a poll firing mid-round-trip, sees `launching` and refuses
    /// instead of creating a second session.
    pub fn claim_launch(&mut self, id: u64, now: i64) -> Result<Task, String> {
        match self.status_of(id) {
            TaskStatus::Launching => return Err("任务正在启动中".into()),
            TaskStatus::Running => return Err("任务已在运行".into()),
            _ => {}
        }
        let task = self
            .tasks
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(|| format!("任务 {id} 不存在"))?;
        self.runtime.insert(
            id,
            Runtime {
                status: TaskStatus::Launching,
                since: now,
            },
        );
        // remember the run even if the launch dies instantly
        if let Some(t) = self.tasks.iter_mut().find(|t| t.id == id) {
            t.last_run_at = Some(now);
        }
        Ok(task)
    }

    /// The launch round trip failed (tmux refused, WSL down): back to a
    /// terminal state so the button unblocks.
    pub fn launch_failed(&mut self, id: u64, now: i64) {
        self.runtime.insert(
            id,
            Runtime {
                status: TaskStatus::Finished,
                since: now,
            },
        );
    }

    /// Book a stop (kill-session) before the round trip; the next reconcile
    /// would reach `finished` anyway, this just makes it immediate and
    /// idempotent under double clicks.
    pub fn claim_stop(&mut self, id: u64, now: i64) -> Result<Task, String> {
        match self.status_of(id) {
            TaskStatus::Launching | TaskStatus::Running => {}
            _ => return Err("任务未在运行".into()),
        }
        let task = self
            .tasks
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(|| format!("任务 {id} 不存在"))?;
        self.runtime.insert(
            id,
            Runtime {
                status: TaskStatus::Finished,
                since: now,
            },
        );
        Ok(task)
    }

    /// Fold one poll's tmux facts into the task statuses:
    /// launching + session alive → running; running + session gone →
    /// finished; launching past the grace window without ever being seen →
    /// finished (instant-failed launch). Linked claude sessions are resolved
    /// for the views only — task sessions take part in monitoring exactly
    /// like hand-started ones, by pid.
    pub fn reconcile(&mut self, tmux_sessions: &[String], now: i64) {
        for task in &self.tasks {
            let name = task.session_name();
            let alive = tmux_sessions.contains(&name);
            let entry = self.runtime.entry(task.id);
            match entry {
                std::collections::hash_map::Entry::Vacant(v) => {
                    // a store loaded mid-flight (app restart while the task
                    // runs): adopt the live session as running
                    if alive {
                        v.insert(Runtime {
                            status: TaskStatus::Running,
                            since: now,
                        });
                    }
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    let rt = o.get_mut();
                    match rt.status {
                        TaskStatus::Launching => {
                            if alive {
                                rt.status = TaskStatus::Running;
                                rt.since = now;
                            } else if now - rt.since >= LAUNCH_GRACE_SECS {
                                rt.status = TaskStatus::Finished;
                                rt.since = now;
                            }
                        }
                        TaskStatus::Running => {
                            if !alive {
                                rt.status = TaskStatus::Finished;
                                rt.since = now;
                            }
                        }
                        TaskStatus::Idle | TaskStatus::Finished => {}
                    }
                }
            }
        }
    }

    /// Views for the UI, ordered running → idle → finished (the launch
    /// surface first), newest run first within a group.
    pub fn views(&self, sessions: &[RawSession]) -> Vec<TaskView> {
        let mut views: Vec<TaskView> = self
            .tasks
            .iter()
            .map(|t| {
                let linked = sessions.iter().find(|s| {
                    s.tmux
                        .as_ref()
                        .map(|m| m.session == t.session_name())
                        .unwrap_or(false)
                });
                TaskView {
                    id: t.id,
                    title: t.title.clone(),
                    cwd: t.cwd.clone(),
                    command: t.command.clone(),
                    status: self.status_of(t.id),
                    session_name: t.session_name(),
                    session_id: linked
                        .map(|s| s.session_id.clone())
                        .filter(|s| !s.is_empty()),
                    pid: linked.map(|s| s.pid),
                    created_at: t.created_at,
                    last_run_at: t.last_run_at,
                }
            })
            .collect();
        let rank = |v: &TaskView| match v.status {
            TaskStatus::Running | TaskStatus::Launching => 0,
            TaskStatus::Idle => 1,
            TaskStatus::Finished => 2,
        };
        views.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then(b.last_run_at.unwrap_or(0).cmp(&a.last_run_at.unwrap_or(0)))
                .then(a.id.cmp(&b.id))
        });
        views
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detector::TmuxInfo;

    fn store() -> TaskStore {
        TaskStore::load(Path::new("/nonexistent/tasks.json"))
    }

    fn task(title: &str, cwd: &str, command: &str) -> Task {
        Task {
            title: title.into(),
            cwd: cwd.into(),
            command: command.into(),
            ..Default::default()
        }
    }

    fn claude_in(session: &str, session_id: &str, pid: i32) -> RawSession {
        RawSession {
            pid,
            cwd: "/tmp/x".into(),
            tty: "/dev/pts/3".into(),
            tmux: Some(TmuxInfo {
                pane: "%9".into(),
                session: session.into(),
                window: "0.0".into(),
            }),
            transcript: None,
            session_id: session_id.into(),
            last_type: String::new(),
            last_ts: String::new(),
            idle_sec: None,
            preview: String::new(),
            usage: None,
            tool_running: false,
            tool_name: String::new(),
            turn_complete: false,
            thinking: false,
            transcript_live: false,
            usage_limited: false,
            resume_at: None,
            subagent_idle_sec: None,
        }
    }

    #[test]
    fn add_assigns_ids_and_rejects_bad_input() {
        let mut s = store();
        let a = s
            .add(task("修复", "~/work/webapp", "claude \"x\""), 100)
            .unwrap();
        let b = s
            .add(task("报告", "/tmp/docs", "make report"), 101)
            .unwrap();
        assert_eq!((a.id, b.id), (1, 2), "ids are sequential and app-owned");
        assert_eq!(a.session_name(), "clawmon-task-1");

        assert!(s.add(task("", "/a", "x"), 102).is_err(), "empty title");
        assert!(
            s.add(task("t", "work/x", "x"), 102).is_err(),
            "relative cwd"
        );
        assert!(s.add(task("t", "/a", "  "), 102).is_err(), "blank command");
        assert!(
            s.add(task("t", "/a\nrm -rf", "x"), 102).is_err(),
            "control chars in cwd"
        );
    }

    #[test]
    fn persistence_roundtrip_and_garbage_dropping() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("tasks.json");
        let mut s = TaskStore::load(&path);
        s.add(task("one", "/a", "cmd1"), 10).unwrap();
        s.add(task("two", "/b", "cmd2"), 11).unwrap();
        s.remove(1).unwrap();
        s.save().unwrap();

        let loaded = TaskStore::load(&path);
        assert_eq!(loaded.tasks().len(), 1);
        assert_eq!(loaded.tasks()[0].title, "two");
        // next_id survived the delete: no id reuse
        let c = loaded_add(loaded, "three", "/c", "cmd3");
        assert_eq!(c.id, 3);

        // hand-edited garbage entries are dropped, valid ones kept
        std::fs::write(
            &path,
            r#"{"nextId":5,"tasks":[
                {"id":4,"title":"ok","cwd":"/ok","command":"true","createdAt":1},
                {"id":5,"title":"bad","cwd":"relative","command":"x","createdAt":1}
            ]}"#,
        )
        .unwrap();
        let s2 = TaskStore::load(&path);
        assert_eq!(s2.tasks().len(), 1);
        assert_eq!(s2.tasks()[0].id, 4);
        assert_eq!(s2.next_id, 5);
    }

    fn loaded_add(mut s: TaskStore, title: &str, cwd: &str, cmd: &str) -> Task {
        s.add(task(title, cwd, cmd), 0).unwrap()
    }

    /// The claim-then-execute invariant (FR10.2): the first claim wins, the
    /// second — overlapping click or poll mid-round-trip — is refused.
    #[test]
    fn launch_is_claimed_exactly_once() {
        let mut s = store();
        s.add(task("t", "/a", "x"), 0).unwrap();
        let spec = s.claim_launch(1, 100).unwrap();
        assert_eq!(spec.session_name(), "clawmon-task-1");
        assert_eq!(s.status_of(1), TaskStatus::Launching);
        assert_eq!(
            s.claim_launch(1, 101).unwrap_err(),
            "任务正在启动中",
            "overlapping click while the round trip is in flight"
        );
        assert_eq!(s.tasks()[0].last_run_at, Some(100));
    }

    #[test]
    fn launch_on_running_task_is_refused() {
        let mut s = store();
        s.add(task("t", "/a", "x"), 0).unwrap();
        s.claim_launch(1, 100).unwrap();
        s.reconcile(&["clawmon-task-1".to_string()], 105);
        assert_eq!(s.status_of(1), TaskStatus::Running);
        assert_eq!(s.claim_launch(1, 106).unwrap_err(), "任务已在运行");
        // relaunch after it ends is fine
        s.reconcile(&[], 110);
        assert_eq!(s.status_of(1), TaskStatus::Finished);
        assert!(s.claim_launch(1, 120).is_ok());
    }

    #[test]
    fn reconcile_lifecycle() {
        let mut s = store();
        s.add(task("t", "/a", "x"), 0).unwrap();

        // launching, session not seen yet, inside grace → stays launching
        s.claim_launch(1, 1000).unwrap();
        s.reconcile(&[], 1010);
        assert_eq!(s.status_of(1), TaskStatus::Launching);

        // session appears (with a claude session inside) → running + linked
        s.reconcile(&["clawmon-task-1".into()], 1020);
        assert_eq!(s.status_of(1), TaskStatus::Running);
        let v = &s.views(&[claude_in("clawmon-task-1", "sess-42", 777)])[0];
        assert_eq!(v.session_id.as_deref(), Some("sess-42"));
        assert_eq!(v.pid, Some(777));

        // session gone → finished
        s.reconcile(&[], 1030);
        assert_eq!(s.status_of(1), TaskStatus::Finished);
    }

    /// A launch that never shows a session (bad dir, instant exit) must not
    /// wedge the button in "launching" forever.
    #[test]
    fn launch_past_grace_is_finished() {
        let mut s = store();
        s.add(task("t", "/a", "x"), 0).unwrap();
        s.claim_launch(1, 1000).unwrap();
        s.reconcile(&[], 1000 + LAUNCH_GRACE_SECS);
        assert_eq!(s.status_of(1), TaskStatus::Finished);
    }

    /// App restart while the task runs: the store comes back empty but the
    /// session is alive — reconcile adopts it instead of showing "idle".
    #[test]
    fn restart_adopts_a_live_session() {
        let mut s = store();
        s.add(task("t", "/a", "x"), 0).unwrap();
        s.reconcile(&["clawmon-task-1".into()], 500);
        assert_eq!(s.status_of(1), TaskStatus::Running);
    }

    #[test]
    fn stop_is_booked_then_terminal() {
        let mut s = store();
        s.add(task("t", "/a", "x"), 0).unwrap();
        assert!(s.claim_stop(1, 100).is_err(), "idle task cannot stop");
        s.claim_launch(1, 100).unwrap();
        s.reconcile(&["clawmon-task-1".into()], 105);
        s.claim_stop(1, 110).unwrap();
        assert_eq!(s.status_of(1), TaskStatus::Finished);
        assert!(s.claim_stop(1, 111).is_err(), "double stop refused");
    }

    #[test]
    fn views_order_running_first() {
        let mut s = store();
        s.add(task("idle", "/a", "x"), 0).unwrap();
        s.add(task("run", "/b", "y"), 0).unwrap();
        s.add(task("done", "/c", "z"), 0).unwrap();
        s.claim_launch(2, 100).unwrap();
        s.reconcile(&["clawmon-task-2".into()], 105); // task 2 → running
        s.claim_launch(3, 70).unwrap();
        // one monotonic clock: task 2 stays alive, task 3's never-seen
        // launch is past the grace window → finished
        s.reconcile(&["clawmon-task-2".into()], 200);
        let order: Vec<u64> = s.views(&[]).iter().map(|v| v.id).collect();
        assert_eq!(order, [2, 1, 3], "running → idle → finished");
    }

    /// Mirrors `session_view_serializes_the_wire_contract` (D10): the exact
    /// key set TaskView hands to ui/app.js, so a Rust rename cannot drift
    /// past CI.
    #[test]
    fn task_view_serializes_the_wire_contract() {
        let mut s = store();
        s.add(task("修复", "~/w", "claude \"go\""), 10).unwrap();
        s.claim_launch(1, 20).unwrap();
        s.reconcile(&["clawmon-task-1".into()], 25);
        let v = &s.views(&[claude_in("clawmon-task-1", "abc", 9)])[0];
        let obj = serde_json::to_value(v).unwrap();
        let mut keys: Vec<&str> = obj
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys.join(","),
            "command,createdAt,cwd,id,lastRunAt,pid,sessionId,sessionName,status,title"
        );
        assert_eq!(obj["status"], "running");
        assert_eq!(obj["sessionName"], "clawmon-task-1");
        assert_eq!(obj["sessionId"], "abc");
        assert_eq!(obj["pid"], 9);
    }
}
