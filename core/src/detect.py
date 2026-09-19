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
public class ClawmonCon {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  public struct RECT { public int L,T,R,B; }
}
"@
Get-Process wsl -ErrorAction SilentlyContinue | ForEach-Object {
  $h = $_.MainWindowHandle
  if ($h -eq [IntPtr]::Zero) { return }
  $r = New-Object ClawmonCon+RECT
  [void][ClawmonCon]::GetWindowRect($h, [ref]$r)
  $c = New-Object Text.StringBuilder 64
  [void][ClawmonCon]::GetClassName($h, $c, 64)
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


_first_ts_cache = {}


def first_ts_epoch(path):
    """Epoch seconds of the first timestamped entry in a transcript.

    A fresh session writes this within seconds of the claude process
    starting, which lets us pair processes to transcript files.
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


def assign_transcripts(procs):
    """Map each claude process to its own transcript file.

    Multiple claude processes can share one working directory, so "newest
    file in the project dir" is not enough. Resolution order:
      1. `--session-id <uuid>` in the cmdline → <uuid>.jsonl (exact)
      2. `--resume <path>` in the cmdline → that file
      3. candidates in the project dir, pairing each process with the file
         whose first timestamped entry best matches the process start time
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
            res = cmd_opt(p["cmd"], "--resume")
            if res and os.path.isfile(res):
                t = res
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
            if not free:
                break
            pick = None
            if len(free) > 1 and p["start"]:
                # prefer the file whose first entry best matches the
                # process start time (fresh sessions start writing
                # within seconds of spawning)
                best = min(free, key=lambda f: abs(
                    (first_ts_epoch(f) or 0) - p["start"]))
                if first_ts_epoch(best) is not None \
                        and abs(first_ts_epoch(best) - p["start"]) <= 300:
                    pick = best
            if pick is None:
                pick = max(free, key=os.path.getmtime)
            claimed.add(pick)
            free.remove(pick)
            result[p["pid"]] = pick

        # A process outlives its transcript when the user /clears: the
        # retired file keeps the birth-time evidence that matched it, so
        # the pass above would pair the process with a closed session
        # forever — its idle time frozen at the last pre-clear entry. Hand
        # such processes over to a newer-born unclaimed file, but only
        # when the retired file actually closed out (trailing untimestamped
        # marker) and went quiet before the new file was born. Those two
        # conditions are what keep an idle session from adopting a dead
        # neighbour's leftover transcript.
        for p in group:
            pick = result.get(p["pid"])
            if not pick:
                continue
            pick_first = first_ts_epoch(pick)
            if pick_first is None or not final_marker_tail(pick):
                continue
            try:
                pick_quiet = os.path.getmtime(pick)
            except OSError:
                continue
            takeover = None
            for f in free:
                f_first = first_ts_epoch(f)
                if f_first is None or f_first <= pick_first:
                    continue
                if pick_quiet > f_first + 60:
                    continue  # the pick was still live after f was born
                if takeover is None or f_first > first_ts_epoch(takeover):
                    takeover = f
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
            if f in claimed:
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


def final_marker_tail(path):
    """True when the transcript ends with a session close-out record.

    /clear (and a clean exit) retire a transcript by appending one last
    untimestamped meta record (`cost-state`, `atis-latch`, ...); every
    in-session entry carries a timestamp, so a session still open — even
    one idle at the prompt — ends on a timestamped one. This is the
    fingerprint that tells "cleared" apart from "merely idle".
    """
    try:
        tail = read_tail(path)
    except OSError:
        return False
    lines = [l for l in tail.split("\n") if l.strip()]
    if not lines:
        return False
    try:
        d = json.loads(lines[-1])
    except Exception:
        return False  # mid-write or non-JSON trailer: not a close-out
    return isinstance(d, dict) and "type" in d and "timestamp" not in d


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


def last_entry(path):
    """Parse the last complete JSON line of the transcript.

    Returns dict with type, timestamp, session_id, preview, tool_running,
    tool_name, turn_complete, thinking, usage_limited, resume_at.
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
    """
    try:
        tail = read_tail(path)
    except OSError:
        return None
    lines = [l for l in tail.split("\n") if l.strip()][-MAX_SCAN_LINES:]
    entry = None
    session_id = None
    preview = ""
    turn_complete = False
    # walk from the end; trailers after the last turn mean it finished.
    # Keep the last user/assistant line as the entry, and also grab the
    # nearest assistant text for a preview.
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
        if entry is None and d.get("type") == "system" \
                and d.get("subtype") in _TURN_TRAILERS:
            turn_complete = True
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
    if entry.get("type") == "assistant":
        full = _blocks_text((entry.get("message") or {}).get("content") or [])
        usage_limited, resume_at = parse_usage_limit(entry, full)
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
        sessions.append(info)
    sessions.sort(key=lambda s: s["pid"])
    return {"now": time_now.isoformat(),
            "now_epoch": time_now.timestamp(),
            "sessions": sessions}


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
