#!/usr/bin/env python3
"""WSL-side Claude Code session detector.

Runs inside WSL. Two modes:
  one-shot (default): `python3 -` with this script on stdin — print one JSON
    object describing every live Claude Code process, then exit.
  resident (`--serve`): stay alive and run one scan per non-empty stdin
    line, answering with one JSON line per request. The Windows side keeps
    this process across polls — one resident python3 is much cheaper than a
    fresh wsl.exe boot per poll.

No third-party dependencies; Python 3.6+ stdlib only.
"""
import glob
import json
import os
import re
import subprocess
import sys
import time
from datetime import datetime, timezone

HOME = os.path.expanduser("~")
CLAUDE_DIR = os.path.join(HOME, ".claude", "projects")
try:
    TICKS = os.sysconf("SC_CLK_TCK")
except Exception:
    TICKS = 100
_BOOT = [None]

# How much of a transcript to decode when looking for its last entry, and how
# far back we are willing to walk for it.
TAIL_WINDOW = 64 * 1024
TAIL_WINDOW_MAX = 1024 * 1024
# Only the last few entries matter; bounds the work on a long window.
MAX_SCAN_LINES = 200


def read_cmdline(pid):
    try:
        with open("/proc/%d/cmdline" % pid, "rb") as f:
            return [t.decode("utf-8", "replace") for t in f.read().split(b"\0") if t]
    except OSError:
        return []


_status_cache = {}


def read_status(pid):
    """Return dict of /proc/<pid>/status key/values (we need PPid)."""
    if pid in _status_cache:
        return _status_cache[pid]
    info = {}
    try:
        with open("/proc/%d/status" % pid) as f:
            for line in f:
                if ":" in line:
                    k, v = line.split(":", 1)
                    info[k] = v.strip()
    except OSError:
        pass
    # ancestor chains overlap heavily between claude processes; one snapshot
    # per run is plenty and saves a /proc read per process.
    _status_cache[pid] = info
    return info


def is_interactive_tty(tty):
    """True when stdin is a live terminal the user could still type in.

    Closing a WSL / Windows Terminal tab shuts the pty master. A process
    that ignores SIGHUP (typical of node, which Claude Code is) keeps
    running; its fd 0 then reads as `/dev/pts/N (deleted)`. A prefix
    match alone would keep listing that ghost as a session.

    A second ghost shape is handled separately (`orphaned_wsl_starts`):
    Windows Terminal closes the tab but WSL's Relay keeps the master
    open, so the slave pts still exists. The leftover `wsl.exe` then
    holds only a 0x0 PseudoConsoleWindow — see `collect` pass 1c.
    """
    if not tty or " (deleted)" in tty:
        return False
    if not tty.startswith(("/dev/pts/", "/dev/tty", "/dev/console")):
        return False
    return os.path.exists(tty)


def read_comm(pid):
    try:
        with open("/proc/%d/comm" % pid) as f:
            return f.read().strip()
    except OSError:
        return ""


def session_leader_start(pid):
    """Start epoch of the WSL SessionLeader that owns `pid`, if any.

    Used to pair a Linux session with the Windows `wsl.exe` that spawned
    it (their start times land within a second). Falls back to `pid`'s
    own start when this is not a WSL login tree.
    """
    born = proc_start_epoch(pid)
    seen = set()
    cur = pid
    for _ in range(64):
        if read_comm(cur).startswith("SessionLeader"):
            return proc_start_epoch(cur) or born
        st = read_status(cur)
        pp = st.get("PPid")
        if not pp or not pp.isdigit():
            break
        pp = int(pp)
        if pp <= 1 or pp in seen:
            break
        seen.add(pp)
        cur = pp
    return born


# Closing a Windows Terminal tab often leaves `wsl.exe` holding a 0x0
# PseudoConsoleWindow while the Linux process (node ignores SIGHUP) and
# the WSL Relay keep the pts alive. Match those leftovers by start time.
# A live WT tab's wsl.exe has handle 0 (the window is on WindowsTerminal)
# and must not be treated as orphaned.
_POWERSHELL = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe"
_ORPHAN_SLOP_SECS = 5
_ORPHAN_TTL_SECS = 15
_orphan_cache = {"at": 0.0, "starts": None}

_ORPHAN_PS = r"""
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class CcMonitorCon {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  public struct RECT { public int L,T,R,B; }
}
"@
Get-Process wsl -ErrorAction SilentlyContinue | ForEach-Object {
  $h = $_.MainWindowHandle
  if ($h -eq [IntPtr]::Zero) { return }
  $r = New-Object CcMonitorCon+RECT
  [void][CcMonitorCon]::GetWindowRect($h, [ref]$r)
  $c = New-Object Text.StringBuilder 64
  [void][CcMonitorCon]::GetClassName($h, $c, 64)
  if ($c.ToString() -eq 'PseudoConsoleWindow' -and ($r.R - $r.L) -eq 0 -and ($r.B - $r.T) -eq 0) {
    ([DateTimeOffset]$_.StartTime.ToUniversalTime()).ToUnixTimeSeconds()
  }
}
"""


def matches_orphaned_console(born, orphaned, slop=_ORPHAN_SLOP_SECS):
    """True when `born` is a WSL SessionLeader start for a closed tab."""
    if born is None or not orphaned:
        return False
    for o in orphaned:
        if abs(born - o) <= slop:
            return True
    return False


def orphaned_wsl_starts():
    """Epoch seconds of leftover `wsl.exe` 0x0 PseudoConsole windows.

    Empty when Windows is unreachable (CI, no interop) — we then only
    have the `(deleted)` pts check. Cached so a poll does not pay a
    fresh powershell boot every time.
    """
    now = time.time()
    if _orphan_cache["starts"] is not None \
            and now - _orphan_cache["at"] < _ORPHAN_TTL_SECS:
        return _orphan_cache["starts"]
    starts = _query_orphaned_wsl_starts()
    _orphan_cache["at"] = now
    _orphan_cache["starts"] = starts
    return starts


def _query_orphaned_wsl_starts():
    if not os.path.isfile(_POWERSHELL):
        return []
    try:
        out = subprocess.run(
            [_POWERSHELL, "-NoProfile", "-Command", _ORPHAN_PS],
            capture_output=True, text=True, timeout=8)
    except Exception:
        return []
    if out.returncode != 0:
        return []
    starts = []
    for line in out.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            starts.append(float(line))
        except ValueError:
            continue
    return starts


def is_claude(cmd):
    if not cmd:
        return False
    t0 = os.path.basename(cmd[0])
    if t0 == "claude":
        return True
    if t0 in ("node", "nodejs"):
        for a in cmd[1:]:
            if a.endswith("/claude") or a.endswith("\\claude") or a.endswith("/claude.js") \
                    or a.endswith("claude-code/cli.js") or a.endswith("claude-code/cli.mjs"):
                return True
    return False


def ancestors(pid):
    """Set of ancestor pids (excluding init)."""
    seen = set()
    cur = pid
    for _ in range(64):  # cycle guard
        st = read_status(cur)
        pp = st.get("PPid")
        if not pp or not pp.isdigit():
            break
        pp = int(pp)
        if pp <= 1 or pp in seen:
            break
        seen.add(pp)
        cur = pp
    return seen


def tmux_panes():
    """{pane_pid: {pane, session, window}} or {} when no tmux server."""
    try:
        out = subprocess.run(
            ["tmux", "list-panes", "-a", "-F",
             "#{pane_id}\t#{pane_pid}\t#{session_name}\t#{window_index}.#{pane_index}"],
            capture_output=True, text=True, timeout=5)
        if out.returncode != 0:
            return {}
        panes = {}
        for line in out.stdout.splitlines():
            parts = line.split("\t")
            if len(parts) != 4:
                continue
            pane_id, pane_pid, session, window = parts
            if pane_pid.isdigit():
                panes[int(pane_pid)] = {
                    "pane": pane_id, "session": session, "window": window}
        return panes
    except Exception:
        return {}


def cmd_opt(cmd, name):
    """Value of `--name value` or `--name=value` in a cmdline, or None."""
    for i, a in enumerate(cmd):
        if a == name and i + 1 < len(cmd):
            return cmd[i + 1]
        if a.startswith(name + "="):
            return a[len(name) + 1:]
    return None


def boot_epoch():
    if _BOOT[0] is None:
        try:
            with open("/proc/uptime") as f:
                up = float(f.read().split()[0])
            _BOOT[0] = time.time() - up
        except Exception:
            _BOOT[0] = 0
    return _BOOT[0]


def proc_start_epoch(pid):
    """Process start time as epoch seconds (from /proc/<pid>/stat)."""
    try:
        with open("/proc/%d/stat" % pid) as f:
            st = f.read().rsplit(") ", 1)[1].split()
        starttime = int(st[19])  # field 22 overall
        return boot_epoch() + starttime / TICKS
    except Exception:
        return None


def proc_cpu_ticks(pid):
    """utime + stime (fields 14 and 15), or None if the process is gone."""
    try:
        with open("/proc/%d/stat" % pid) as f:
            st = f.read().rsplit(") ", 1)[1].split()
        return int(st[11]) + int(st[12])
    except (OSError, ValueError, IndexError):
        return None


# Resident mode keeps the previous sample so a poll can report ticks/s.
# One-shot mode has no previous sample and always reports None — the
# engine then classifies from the transcript alone.
_SERVE = False
_cpu_prev = {}


def cpu_rate(pid):
    """Ticks/s of user+system time since the previous resident scan.

    None in one-shot mode, on the first sighting of a pid, when the
    counter went backwards (the pid was reused), and when /proc cannot
    be read. Measured on Claude Code 2.1.280 (CLK_TCK 100): idle at the
    prompt is 0–2 ticks/s; a streaming turn is 10–25.
    """
    now_ticks = proc_cpu_ticks(pid)
    now = time.monotonic()
    if not _SERVE or now_ticks is None:
        if now_ticks is None:
            _cpu_prev.pop(pid, None)
        return None
    prev = _cpu_prev.get(pid)
    _cpu_prev[pid] = (now_ticks, now)
    if prev is None or now_ticks < prev[0]:
        return None
    dt = now - prev[1]
    if dt <= 0.05:
        return None
    return (now_ticks - prev[0]) / dt


_first_ts_cache = {}


def first_ts_epoch(path):
    """Epoch seconds of the first timestamped entry in a transcript.

    A fresh session writes this within seconds of the claude process
    starting, which lets us pair processes to transcript files.

    NOT a birth date: an auto-compact continuation opens with the summarized
    history carried over, so its first timestamp can predate the file it
    replaced. Compare content timelines (last_ts_epoch) instead of file
    order when following a retired transcript.
    """
    if path in _first_ts_cache:
        return _first_ts_cache[path]
    ts = None
    try:
        with open(path, "rb") as fh:
            for _ in range(40):  # timestamped entry comes early
                line = fh.readline()
                if not line:
                    break
                line = line.strip()
                if not line.startswith(b"{"):
                    continue
                try:
                    d = json.loads(line)
                except Exception:
                    continue
                t = d.get("timestamp")
                if t:
                    ts = parse_ts(t)
                    break
    except OSError:
        pass
    _first_ts_cache[path] = ts
    return ts


_last_ts_cache = {}


def last_ts_epoch(path):
    """Epoch seconds of the newest timestamped entry, or None.

    Unlike the first timestamp, this moves as the file grows — collect()
    clears the cache every run.
    """
    if path in _last_ts_cache:
        return _last_ts_cache[path]
    ts = None
    try:
        tail = read_tail(path)
    except OSError:
        tail = ""
    for line in reversed(tail.split("\n")):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            t = json.loads(line).get("timestamp")
        except Exception:
            continue
        if t:
            ts = parse_ts(t)
            break
    _last_ts_cache[path] = ts
    return ts


def parse_ts(t):
    try:
        dt = datetime.fromisoformat(t.replace("Z", "+00:00"))
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt.timestamp()
    except Exception:
        return None


def slug_for(path):
    return re.sub(r"[^a-zA-Z0-9]", "-", path)


def scan_transcripts():
    """All transcript jsonl files under ~/.claude/projects, by mtime."""
    files = []
    if os.path.isdir(CLAUDE_DIR):
        for f in glob.glob(os.path.join(CLAUDE_DIR, "*", "*.jsonl")):
            try:
                files.append((os.path.getmtime(f), f))
            except OSError:
                continue
    files.sort(reverse=True)
    return files


# A transcript may be timestamped a little before /proc says the process
# started (clock granularity, the first write racing the stat read). Wider
# than that, a file was born with a different process.
_BIRTH_BEFORE_SECS = 30
# And the first entry lands within seconds of spawn when the session
# starts by being prompted. Later than this, "born with the process" is
# no longer the story — see choose_transcript.
_BIRTH_AFTER_SECS = 15
# mtime is second-resolution and can lag the start we read from /proc.
_MTIME_SLOP_SECS = 5


def file_mtime(path):
    try:
        return os.path.getmtime(path)
    except OSError:
        return None


def written_since_start(path, start):
    """False when the file was last written before this process existed.

    Such a file cannot be its transcript: the fallback heuristics used to
    hand a fresh process the previous session (closed by /exit a couple of
    minutes earlier) because that file's first timestamp was the closest
    match, and the row then stuck on yellow "记录未就绪".
    """
    if not start:
        return True
    mtime = file_mtime(path)
    return mtime is not None and mtime >= start - _MTIME_SLOP_SECS


def choose_transcript(start, free):
    """Which of `free` belongs to the process that started at `start`.

    1. Born with the process — first timestamp within a few seconds of the
       start. That file beats a neighbour that started later in the same
       directory (an idle session must keep its own transcript).
    2. Otherwise a file born after the start, nearest first. No window cap:
       a fresh session writes nothing until the first prompt, which can be
       many minutes after the process.
    3. Otherwise the newest mtime — a resumed session whose transcript
       predates the process but is still being written.
    """
    if start and len(free) > 1:
        def gap(f):
            ft = first_ts_epoch(f)
            return None if ft is None else ft - start

        born = [f for f in free
                if gap(f) is not None
                and -_BIRTH_BEFORE_SECS <= gap(f) <= _BIRTH_AFTER_SECS]
        if born:
            return min(born, key=lambda f: abs(gap(f)))
        after = [f for f in free if gap(f) is not None and gap(f) > 0]
        if after:
            return min(after, key=gap)
    return max(free, key=lambda f: file_mtime(f) or 0)


def assign_transcripts(procs):
    """Map each claude process to its own transcript file.

    Multiple claude processes can share one working directory, so "newest
    file in the project dir" is not enough. Resolution order:
      1. `--session-id <uuid>` in the cmdline → <uuid>.jsonl (exact)
      2. `--resume` / `-r` <path or uuid> in the cmdline → that file
      3. candidates in the project dir that were still being written when
         the process started (see `choose_transcript`)
      4. fallback: newest unclaimed file (then the cwd tail-match scan)
    Files are claimed so no two processes share a transcript.
    Returns {pid: transcript_path or None}.
    """
    claimed = set()
    result = {}

    # pass 1: exact cmdline evidence
    for p in procs:
        t = None
        sid = cmd_opt(p["cmd"], "--session-id")
        if sid:
            cand = os.path.join(CLAUDE_DIR, slug_for(p["cwd"]),
                                sid + ".jsonl")
            if os.path.isfile(cand):
                t = cand
        if t is None:
            res = cmd_opt(p["cmd"], "--resume") or cmd_opt(p["cmd"], "-r")
            if res and os.path.isfile(res):
                t = res
            elif res and p["cwd"]:
                cand = os.path.join(
                    CLAUDE_DIR, slug_for(p["cwd"]), res + ".jsonl")
                if os.path.isfile(cand):
                    t = cand
        if t is not None:
            claimed.add(t)
            result[p["pid"]] = t
        else:
            result[p["pid"]] = None

    # group the still-unmapped processes by project dir
    by_dir = {}
    for p in procs:
        if result[p["pid"]] is None and p["cwd"]:
            by_dir.setdefault(slug_for(p["cwd"]), []).append(p)

    for slug, group in by_dir.items():
        files = glob.glob(os.path.join(CLAUDE_DIR, slug, "*.jsonl"))
        free = [f for f in files if f not in claimed]
        # newest process first: it is the most likely to still be writing
        group.sort(key=lambda p: p["start"] or 0, reverse=True)
        for p in group:
            # a file quiet since before this process started belongs to an
            # earlier session; it must not compete (and must not win on
            # "closest first timestamp")
            eligible = [f for f in free if written_since_start(f, p["start"])]
            if not eligible:
                continue
            pick = choose_transcript(p["start"], eligible)
            claimed.add(pick)
            free.remove(pick)
            result[p["pid"]] = pick

        # A process outlives its transcript when the user /clears or the
        # context auto-compacts: the retired file keeps the evidence that
        # matched it, so the pass above would pair the process with a closed
        # session forever — its idle time frozen at the last pre-retirement
        # entry (observed live: a finished session stuck on green
        # "waiting for API" with a growing idle). Hand such processes over
        # to the unclaimed file that CONTINUES the story: its newest
        # timestamped entry must postdate the retired file's quiet moment.
        # Content order, not file birth order — a compact continuation
        # reopens with backdated summarized history, so comparing first
        # timestamps rejects the very file that took over. Among candidates
        # an open file (no close-out marker) beats a retired one, and newer
        # content wins; the outlives-the-quiet check is what keeps an idle
        # session from adopting a dead neighbour's leftover transcript.
        for p in group:
            pick = result.get(p["pid"])
            if not pick:
                continue
            if first_ts_epoch(pick) is None or not is_retired(pick):
                continue
            try:
                pick_quiet = os.path.getmtime(pick)
            except OSError:
                continue
            takeover, takeover_key = None, None
            for f in free:
                if not written_since_start(f, p["start"]):
                    continue
                f_last = last_ts_epoch(f)
                if f_last is None or f_last < pick_quiet - 5:
                    continue  # does not outlive the retired file
                key = (not is_retired(f), f_last)
                if takeover is None or key > takeover_key:
                    takeover, takeover_key = f, key
            if takeover is not None:
                result[p["pid"]] = takeover
                claimed.add(takeover)
                free.remove(takeover)
                # the retired file stays claimed on purpose: a closed
                # session must not become a candidate for other processes

    # last resort: cwd tail-match scan for processes with nothing so far
    cutoff = 7 * 86400
    recent = [(m, f) for m, f in scan_transcripts() if m > cutoff][:60]
    for p in procs:
        if result[p["pid"]] is not None or not p["cwd"]:
            continue
        for m, f in recent:
            if f in claimed or not written_since_start(f, p["start"]):
                continue
            try:
                with open(f, "rb") as fh:
                    fh.seek(max(0, os.path.getsize(f) - 8192))
                    tail = fh.read().decode("utf-8", "replace")
            except OSError:
                continue
            cwds = re.findall(r'"cwd"\s*:\s*"((?:[^"\\]|\\.)*)"', tail)
            if cwds and cwds[-1] == p["cwd"]:
                claimed.add(f)
                result[p["pid"]] = f
                break
    return result


def json_unescape(s):
    try:
        return json.loads('"' + s + '"')
    except Exception:
        return s


def transcript_is_live(path, start):
    """True when `path` was written by the process that started at `start`.

    A transcript whose last write predates the process cannot belong to it:
    the fallback pairing heuristics do sometimes hand a fresh process an old
    session file, and such a record looks "idle for hours" — which would be
    reported as a stuck session.
    """
    if not start:
        return True  # start time unknown → cannot rule it out
    try:
        return os.path.getmtime(path) >= start - 5
    except OSError:
        return False


def read_tail(path, window=TAIL_WINDOW, limit=TAIL_WINDOW_MAX):
    """Decode the tail of a file, always starting on a line boundary.

    A single entry can be much larger than the window (a big tool result), and
    a line cut in half parses as nothing — the session would then look like it
    has no transcript at all. Grow the window until the first line is complete.
    """
    size = os.path.getsize(path)
    while True:
        start = max(0, size - window)
        with open(path, "rb") as fh:
            if start:
                fh.seek(start - 1)
                at_boundary = fh.read(1) == b"\n"
            else:
                at_boundary = True
            data = fh.read()
        text = data.decode("utf-8", "replace")
        if at_boundary:
            return text
        newline = text.find("\n")
        if newline >= 0:
            return text[newline + 1:]
        if window >= limit:
            return text  # one line longer than we are willing to buffer
        window *= 2


# Records a local slash command (`/exit`, `/model`, …) appends. They are
# not a turn the model owes a reply to. `/exit` writes them AFTER the
# `cost-state` close-out, so "the last line is untimestamped" does not
# recognise the retirement — those lines are timestamped.
_LOCAL_COMMAND_MARKERS = (
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<local-command-caveat>",
    "<command-name>",
    "<bash-stdout>",
    "<bash-stderr>",
)

_retired_cache = {}


def entry_text(entry):
    """Flatten a record's message content to text."""
    content = (entry.get("message") or {}).get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return _blocks_text(content)
    return ""


def is_local_command_entry(entry):
    """True for a slash command claude handled locally, with no model turn."""
    if not isinstance(entry, dict):
        return False
    if entry.get("type") == "system" and entry.get("subtype") == "local_command":
        return True
    if entry.get("type") != "user":
        return False
    text = entry_text(entry)
    return any(marker in text for marker in _LOCAL_COMMAND_MARKERS)


def is_retired(path):
    """True when the transcript was closed out and not continued.

    `cost-state` is written once, when a session is retired (`/clear`,
    auto-compact, `/exit`). Untimestamped markers (`mode`, `last-prompt`,
    `atis-latch`) also appear mid-session, so they are not a close-out.
    `/exit` appends timestamped local-command records after `cost-state`;
    those do not reopen the session. A real turn after `cost-state` does.
    """
    if path in _retired_cache:
        return _retired_cache[path]
    retired = _is_retired(path)
    _retired_cache[path] = retired
    return retired


def _is_retired(path):
    try:
        tail = read_tail(path)
    except OSError:
        return False
    parsed = []
    for line in tail.split("\n"):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            parsed.append(json.loads(line))
        except Exception:
            continue
    idx = None
    for i, d in enumerate(parsed):
        if isinstance(d, dict) and d.get("type") == "cost-state":
            idx = i
    if idx is None:
        return False
    for d in parsed[idx + 1:]:
        if not isinstance(d, dict):
            continue
        # bookkeeping after the close-out (last-prompt, mode, …) is fine;
        # only a turn entry can mean the session continued
        if d.get("type") not in ("user", "assistant") or "timestamp" not in d:
            continue
        if not is_local_command_entry(d):
            return False
    return True


# Conversation entries we classify on. Claude Code appends timestamped
# `system` records after a finished turn (`turn_duration`, hook summaries);
# those must not hide the assistant/user entry classify actually needs, but
# they ARE the only trustworthy "this turn is over" signal — a trailing
# assistant text block is often just a status line before the next think
# or tool_use.
_TURN_TYPES = ("user", "assistant")
_TURN_TRAILERS = ("turn_duration", "stop_hook_summary")

# Usage-quota 429s (5-hour window, weekly, …) are written as a synthetic
# assistant record plus a turn_duration trailer. The trailer would make
# classify treat them as a finished turn; the markers below are the
# difference between "done" and "paused until the window resets".
_USAGE_LIMIT_MARKERS = ("使用上限", "usage quota", "usage limit")
_RESET_AT = re.compile(
    r"(?:限额将在|reset at)\s+"
    r"(\d{4}-\d{2}-\d{2}\s+\d{2}:\d{2}:\d{2})"
    r"(?:\s+([+-]\d{4}))?",
    re.I,
)


def _blocks_text(content):
    """Join text blocks from a message.content list."""
    if not isinstance(content, list):
        return ""
    parts = []
    for b in content:
        if isinstance(b, dict) and b.get("type") == "text":
            t = (b.get("text") or "").strip()
            if t:
                parts.append(t)
    return "\n".join(parts)


def notice_kind(text):
    """A goal-status line claude writes after the turn, or "".

    "retrying" means claude will try again on its own. "goal_paused" means
    it stopped and asked the user to send a message. Paused wins when a
    line could be read either way.
    """
    if not isinstance(text, str):
        return ""
    low = text.lower()
    if "goal paused" in low or "send a message to continue" in low:
        return "goal_paused"
    if "retrying" in low:
        return "retrying"
    return ""


# Status codes that will not start working because we press Enter.
_FATAL_API_STATUS = (400, 401, 403, 404)
_RETRYABLE_TEXT = (
    "connection lost",
    "overloaded",
    "timeout",
    "timed out",
    "econnreset",
    "socket hang up",
)


def parse_api_error(entry, text, usage_limited):
    """Return (api_error, retryable) for a non-quota API failure.

    A usage-quota 429 stays on the usage-limit path and is not also an
    API error. Retryable: connection lost, 5xx, 529 overloaded, a
    non-quota 429, a timeout. 400/401/403/404 are not — pressing Enter
    cannot fix a rejected request. Anything we cannot recognise as
    retryable is not: an unknown failure needs a person.
    """
    if usage_limited:
        return False, False
    status = entry.get("apiErrorStatus")
    is_api = (
        bool(entry.get("isApiErrorMessage"))
        or bool(entry.get("error"))
        or status is not None
        or (text or "").lstrip().startswith("API Error:")
    )
    if not is_api:
        return False, False
    if isinstance(status, int) and status in _FATAL_API_STATUS:
        return True, False
    low = (text or "").lower()
    retryable = False
    if isinstance(status, int) and (
            status in (408, 429, 529) or status >= 500):
        retryable = True
    if any(marker in low for marker in _RETRYABLE_TEXT) or "529" in low:
        retryable = True
    return True, retryable


def user_kind_of(entry):
    """What a trailing user record actually is.

    ``prompt`` is the only kind the model owes a reply to. A bare
    ``<command-name>`` stays a prompt: skill and prompt commands do get a
    model reply. The stdout of a local command does not.
    """
    if entry.get("type") != "user":
        return ""
    text = entry_text(entry)
    if "Request interrupted by user" in text:
        return "interrupt"
    if any(marker in text for marker in (
            "<local-command-stdout>", "<local-command-stderr>",
            "<bash-stdout>", "<bash-stderr>", "<local-command-caveat>")):
        return "local_command"
    content = (entry.get("message") or {}).get("content")
    if isinstance(content, list) and any(
            isinstance(b, dict) and b.get("type") == "tool_result"
            for b in content):
        return "tool_result"
    if entry.get("isMeta"):
        return "meta"
    return "prompt"


def parse_usage_limit(entry, text):
    """Return (usage_limited, resume_at_epoch_or_None) for a quota 429.

    Naive reset stamps (Chinese: `限额将在 2026-09-19 16:21:07 重置`) are
    local time — detect.py runs on the same machine that printed them.
    An offset (`+0800`) is taken as-is so English errors stay TZ-stable.
    """
    if not text:
        return False, None
    is_api_err = (
        bool(entry.get("isApiErrorMessage"))
        or entry.get("error") == "rate_limit"
        or entry.get("apiErrorStatus") == 429
        or text.lstrip().startswith("API Error:")
    )
    if not is_api_err or not any(m in text for m in _USAGE_LIMIT_MARKERS):
        return False, None
    resume_at = None
    m = _RESET_AT.search(text)
    if m:
        stamp, tz = m.group(1), m.group(2)
        try:
            if tz:
                dt = datetime.strptime(
                    stamp + " " + tz, "%Y-%m-%d %H:%M:%S %z")
            else:
                dt = datetime.strptime(stamp, "%Y-%m-%d %H:%M:%S")
            resume_at = int(dt.timestamp())
        except ValueError:
            resume_at = None
    return True, resume_at


def last_entry(path, window=TAIL_WINDOW):
    """Parse the last complete JSON line of the transcript.

    Returns dict with type, timestamp, session_id, preview, tool_running,
    tool_name, turn_complete, thinking, usage_limited, resume_at,
    user_kind, api_error, api_error_retryable, system_notice.
    `tool_running` is True when the last timestamped *turn* entry is an
    assistant message containing a tool_use block — the tool result is
    only appended when the tool finishes, so this is exactly "a tool is
    executing now"; `tool_name` is that block's tool. `turn_complete` is
    True when a `turn_duration` / `stop_hook_summary` follows that turn
    (the turn really ended). `thinking` is True when the last turn is an
    assistant message that holds only a thinking block — the model is
    still generating, not waiting on the user. Trailing `system`
    bookkeeping is skipped for `type` but recorded as the trailer.
    `usage_limited` is True when that last turn is a usage-quota 429
    (Claude Code still writes a trailer, but the goal is paused until
    reset); `resume_at` is the parsed reset epoch, or None.
    `user_kind` classifies a trailing user record (prompt, tool_result,
    interrupt, local_command, meta); "" when the last turn is not a user
    record. `api_error` / `api_error_retryable` describe a non-quota API
    failure. `system_notice` is "retrying" or "goal_paused" when a
    goal-status line follows the turn.
    """
    try:
        tail = read_tail(path, window=window)
    except OSError:
        return None
    lines = [l for l in tail.split("\n") if l.strip()][-MAX_SCAN_LINES:]
    entry = None
    session_id = None
    preview = ""
    turn_complete = False
    system_notice = ""
    local_command_after = False
    # walk from the end; trailers after the last turn mean it finished.
    # Keep the last user/assistant line as the entry, and also grab the
    # nearest assistant text for a preview. System lines between the end
    # and that turn are the notices, not the turn itself.
    for line in reversed(lines):
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            d = json.loads(line)
        except Exception:
            continue
        if session_id is None:
            session_id = d.get("sessionId") or d.get("session_id")
        if entry is None and d.get("type") == "system":
            subtype = d.get("subtype") or ""
            if subtype in _TURN_TRAILERS:
                turn_complete = True
                continue
            if subtype == "local_command":
                local_command_after = True
                continue
            if subtype in ("informational", "api_error"):
                kind = notice_kind(d.get("content") or "")
                if kind and not system_notice:
                    system_notice = kind
                continue
        if entry is None and d.get("type") in _TURN_TYPES and "timestamp" in d:
            entry = d
        if not preview and d.get("type") == "assistant":
            msg = d.get("message") or {}
            content = msg.get("content") or []
            for block in content:
                if isinstance(block, dict) and block.get("type") == "text" \
                        and block.get("text", "").strip():
                    preview = block["text"].strip().replace("\n", " ") \
                        .replace("\r", " ")[:160]
                    break
        if entry is not None and preview:
            break
    if entry is None:
        # A line bigger than the window (a large attachment) makes the read
        # start mid-line and drop the turn that sits just before it. Widen
        # and try again, up to the cap.
        if window < TAIL_WINDOW_MAX:
            return last_entry(path, window=min(window * 2, TAIL_WINDOW_MAX))
        return None
    tool_running = False
    tool_name = ""
    thinking = False
    if entry.get("type") == "assistant":
        content = (entry.get("message") or {}).get("content") or []
        has_text = False
        has_thinking = False
        if isinstance(content, list):
            for b in content:
                if not isinstance(b, dict):
                    continue
                kind = b.get("type")
                if kind == "tool_use":
                    tool_running = True
                    tool_name = str(b.get("name") or "")
                    break
                if kind == "text" and b.get("text", "").strip():
                    has_text = True
                if kind == "thinking":
                    has_thinking = True
        thinking = (not tool_running) and has_thinking and not has_text
    usage_limited, resume_at = False, None
    api_error, api_retryable = False, False
    if entry.get("type") == "assistant":
        full = _blocks_text((entry.get("message") or {}).get("content") or [])
        usage_limited, resume_at = parse_usage_limit(entry, full)
        api_error, api_retryable = parse_api_error(entry, full, usage_limited)
    user_kind = user_kind_of(entry)
    if local_command_after:
        user_kind = "local_command"
    return {
        "type": entry.get("type", ""),
        "timestamp": entry.get("timestamp", ""),
        "session_id": session_id or "",
        "preview": preview,
        "tool_running": tool_running,
        "tool_name": tool_name,
        "turn_complete": turn_complete,
        "thinking": thinking,
        "usage_limited": usage_limited,
        "resume_at": resume_at,
        "user_kind": user_kind,
        "api_error": api_error,
        "api_error_retryable": api_retryable,
        "system_notice": system_notice,
    }


# ---- per-session token usage --------------------------------------------
#
# Transcripts write one assistant record per streamed content block, and
# every duplicate repeats the whole `message.usage` of that API request —
# summing naively overcounts 2-3x. So we keep, per file, the LAST usage
# per message id and sum over the ids (unique ids ≈ API requests; ids are
# server-side unique, so parent and subagent maps merge cleanly).
# Subagent transcripts (<session-id>/subagents/agent-*.jsonl) carry the
# same records and count toward the session. In resident mode the cache
# makes repeat scans incremental: only appended bytes are parsed.

_usage_cache = {}


def scan_usage(path):
    """Fold newly appended transcript bytes into the per-file id map."""
    entry = _usage_cache.get(path)
    if entry is None:
        entry = _usage_cache[path] = {"offset": 0, "ids": {}}
    try:
        size = os.path.getsize(path)
    except OSError:
        return entry
    if size < entry["offset"]:
        # truncated or rewritten from scratch — totals must be rebuilt
        entry["offset"] = 0
        entry["ids"] = {}
    if size == entry["offset"]:
        return entry
    with open(path, "rb") as fh:
        fh.seek(entry["offset"])
        data = fh.read()
    end = data.rfind(b"\n")
    if end < 0:
        return entry  # the new bytes hold no complete line yet
    for raw in data[:end].split(b"\n"):
        line = raw.decode("utf-8", "replace").strip()
        if not line.startswith("{"):
            continue
        try:
            d = json.loads(line)
        except Exception:
            continue
        if d.get("type") != "assistant":
            continue
        msg = d.get("message") or {}
        mid = msg.get("id")
        usage = msg.get("usage")
        if not mid or not isinstance(usage, dict):
            continue  # cannot dedupe → summing it is the overcount
        entry["ids"][mid] = usage
    entry["offset"] += end + 1
    return entry


def subagent_activity_files(path):
    """Transcripts a background agent of this session is still writing.

    Direct subagents land at <stem>/subagents/agent-*.jsonl. A dynamic
    workflow (Claude Code: "Waiting for N dynamic workflow(s) to finish")
    lands one level deeper, at
    <stem>/subagents/workflows/<run-id>/agent-*.jsonl, and its journal.jsonl
    moves as phases start and finish. Both are the same in-process wait.
    """
    directory = os.path.dirname(path)
    stem = os.path.splitext(os.path.basename(path))[0]
    base = os.path.join(directory, stem, "subagents")
    patterns = (
        os.path.join(base, "agent-*.jsonl"),
        os.path.join(base, "workflows", "*", "agent-*.jsonl"),
        os.path.join(base, "workflows", "*", "journal.jsonl"),
    )
    found = []
    for pat in patterns:
        found.extend(glob.glob(pat))
    return found


def subagent_idle_sec(path, now):
    """Seconds since the newest subagent transcript write, or None.

    Background agents and dynamic workflows run in-process and write their
    own transcripts (see subagent_activity_files) while the main transcript
    sits on a finished-turn trailer — Claude Code holds the turn open for
    them, so that freshness is the signal that the session waits on agents,
    not on the user (see the engine's WaitingSubagent green). A later local
    slash command such as /workflows does not end the wait.
    """
    newest = None
    for sub in subagent_activity_files(path):
        try:
            m = os.path.getmtime(sub)
        except OSError:
            continue
        if newest is None or m > newest:
            newest = m
    if newest is None:
        return None
    return max(0, int(now - newest))


def transcript_usage(path):
    """Total usage of a session: main transcript + subagent transcripts."""
    states = [scan_usage(path)]
    directory = os.path.dirname(path)
    stem = os.path.splitext(os.path.basename(path))[0]
    for sub in sorted(glob.glob(
            os.path.join(directory, stem, "subagents", "agent-*.jsonl"))):
        states.append(scan_usage(sub))
    ids = {}
    for st in states:
        ids.update(st["ids"])
    if not ids:
        return None

    def _n(d, k):
        v = d.get(k)
        return v if isinstance(v, (int, float)) else 0

    return {
        "input": int(sum(_n(u, "input_tokens") for u in ids.values())),
        "cache_read": int(sum(
            _n(u, "cache_read_input_tokens") for u in ids.values())),
        "cache_creation": int(sum(
            _n(u, "cache_creation_input_tokens") for u in ids.values())),
        "output": int(sum(_n(u, "output_tokens") for u in ids.values())),
        "requests": len(ids),
    }


def collect():
    """One detection pass; returns the status dict without printing.

    In resident mode this runs once per request, so per-run caches must be
    reset here: a cached /proc status would survive a dead pid and poison
    the ancestor walk of whichever process recycles it later.
    """
    _status_cache.clear()
    _retired_cache.clear()
    # last timestamps move with every append, so this cache is strictly
    # per-run (first timestamps are immutable per file and may persist)
    _last_ts_cache.clear()
    # first timestamps are immutable per transcript file, so this cache may
    # live across scans — but it must not grow without bound
    if len(_first_ts_cache) > 4096:
        _first_ts_cache.clear()
    # usage caches are the same kind of state, but subagent files churn
    if len(_usage_cache) > 512:
        _usage_cache.clear()

    time_now = datetime.now(timezone.utc)
    panes = tmux_panes()

    # pass 1: collect every claude process with what /proc can tell us
    procs = []
    for entry_ in os.listdir("/proc"):
        if not entry_.isdigit():
            continue
        pid = int(entry_)
        cmd = read_cmdline(pid)
        if not is_claude(cmd):
            continue
        try:
            cwd = os.path.realpath(os.readlink("/proc/%d/cwd" % pid))
        except OSError:
            cwd = ""
        # terminal
        tty = ""
        try:
            tty = os.readlink("/proc/%d/fd/0" % pid)
        except OSError:
            pass
        # tmux pane membership
        pane_info = None
        if panes:
            if pid in panes:
                pane_info = panes[pid]
            else:
                for anc in ancestors(pid):
                    if anc in panes:
                        pane_info = panes[anc]
                        break
        procs.append({
            "pid": pid,
            "cmd": cmd,
            "cwd": cwd,
            "tty": tty,
            "tmux": pane_info,
            "start": proc_start_epoch(pid),
        })

    # pass 1b: keep only interactive sessions. Plugins and agent SDKs spawn
    # extra claude processes — e.g. a hook running a security review on every
    # tool call, or an SDK's bundled binary. They are children of an existing
    # session, not sessions of their own, and listing them made one terminal
    # show up as many. Two independent signals, either conclusive: the
    # process descends from another claude process, or its stdin is not a
    # terminal (a pipe handed over by the spawner) so there is nothing to
    # monitor or auto-continue. The second also catches agents whose parent
    # already exited and got reparented.
    matched = set(p["pid"] for p in procs)
    procs = [
        p for p in procs
        if not (matched & ancestors(p["pid"]))
        and is_interactive_tty(p["tty"])
    ]

    # pass 1c: drop sessions whose Windows console is a leftover 0x0
    # PseudoConsole (WT tab closed; node ignored SIGHUP; Relay kept the
    # pts so pass 1b still sees an interactive tty). tmux sessions stay
    # — the user can reattach. Fail-open when Windows is unreachable.
    orphaned = orphaned_wsl_starts()
    if orphaned:
        procs = [
            p for p in procs
            if p["tmux"] or not matches_orphaned_console(
                session_leader_start(p["pid"]), orphaned)
        ]

    # pass 2: one transcript per process (concurrent sessions don't share)
    transcripts = assign_transcripts(procs)

    sessions = []
    for p in procs:
        info = {
            "pid": p["pid"],
            "cwd": p["cwd"],
            "tty": p["tty"],
            "tmux": p["tmux"],
            "transcript": transcripts.get(p["pid"]),
            "session_id": "",
            "last_type": "",
            "last_ts": "",
            "idle_sec": None,
            "preview": "",
            "tool_running": False,
            "tool_name": "",
            "turn_complete": False,
            "thinking": False,
            "transcript_live": False,
            "usage_limited": False,
            "resume_at": None,
            "user_kind": "",
            "api_error": False,
            "api_error_retryable": False,
            "system_notice": "",
            "subagent_idle_sec": None,
            "cpu_ticks_per_sec": None,
            "usage": None,
        }
        transcript = info["transcript"]
        if transcript:
            info["transcript_live"] = transcript_is_live(
                transcript, p["start"])
            le = last_entry(transcript)
            if le:
                info["session_id"] = le["session_id"]
                info["last_type"] = le["type"]
                info["last_ts"] = le["timestamp"]
                info["preview"] = le["preview"]
                info["tool_running"] = le["tool_running"]
                info["tool_name"] = le["tool_name"]
                info["turn_complete"] = bool(le.get("turn_complete"))
                info["thinking"] = bool(le.get("thinking"))
                info["usage_limited"] = bool(le.get("usage_limited"))
                info["user_kind"] = le.get("user_kind") or ""
                info["api_error"] = bool(le.get("api_error"))
                info["api_error_retryable"] = bool(le.get("api_error_retryable"))
                info["system_notice"] = le.get("system_notice") or ""
                resume_at = le.get("resume_at")
                info["resume_at"] = int(resume_at) if resume_at is not None else None
                ts = parse_ts(le["timestamp"])
                if ts is not None:
                    info["idle_sec"] = max(
                        0, int(time_now.timestamp() - ts))
            # last_entry can miss (huge mid-write line, only trailers in
            # the window). File mtime still tells us the session is live —
            # leaving idle_sec None made classify treat it as "waiting for
            # input" (yellow) while claude was still working.
            if info["idle_sec"] is None:
                try:
                    info["idle_sec"] = max(
                        0, int(time_now.timestamp() - os.path.getmtime(transcript)))
                except OSError:
                    pass
            try:
                info["usage"] = transcript_usage(transcript)
            except Exception:
                info["usage"] = None  # usage must never cost a poll
            try:
                info["subagent_idle_sec"] = subagent_idle_sec(
                    transcript, time_now.timestamp())
            except Exception:
                pass  # freshness must never cost a poll either
        info["cpu_ticks_per_sec"] = cpu_rate(p["pid"])
        sessions.append(info)
    # drop samples for processes that are gone, so a recycled pid is not
    # compared against the previous owner's counter
    live = {s["pid"] for s in sessions}
    for gone in [pid for pid in _cpu_prev if pid not in live]:
        _cpu_prev.pop(gone, None)
    sessions.sort(key=lambda s: s["pid"])
    # every live tmux session name (not only claude ones): task launches
    # reconcile their liveness against this list each poll
    tmux_sessions = sorted({p["session"] for p in panes.values()})
    return {"now": time_now.isoformat(),
            "now_epoch": time_now.timestamp(),
            "sessions": sessions,
            "tmux_sessions": tmux_sessions}


def emit(scan):
    json.dump(scan, sys.stdout, ensure_ascii=False)
    sys.stdout.write("\n")
    sys.stdout.flush()


def main():
    emit(collect())


def serve():
    """Resident mode: one scan per non-empty stdin line, one JSON line back.

    An internal error is reported as `{"error": ...}` — unparsable as a
    status on the Rust side, which then falls back to a one-shot run (and
    its real error message) instead of silently treating the poll as empty.
    """
    global _SERVE
    _SERVE = True
    for line in sys.stdin:
        cmd = line.strip()
        if cmd == "quit":
            break
        if not cmd:
            continue
        try:
            emit(collect())
        except Exception as e:  # noqa: BLE001 - the driver handles the error
            emit({"error": str(e)})


if __name__ == "__main__":
    if "--serve" in sys.argv[1:]:
        serve()
    else:
        main()
