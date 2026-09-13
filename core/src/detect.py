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


def last_entry(path):
    """Parse the last complete JSON line of the transcript.

    Returns dict with type, timestamp, session_id, preview, tool_running.
    `tool_running` is True when the last timestamped entry is an assistant
    message containing a tool_use block — the tool result is only appended
    when the tool finishes, so this is exactly "a tool is executing now".
    """
    try:
        tail = read_tail(path)
    except OSError:
        return None
    lines = [l for l in tail.split("\n") if l.strip()][-MAX_SCAN_LINES:]
    entry = None
    session_id = None
    preview = ""
    # walk from the end; keep the last parseable line as the entry, and also
    # grab the nearest assistant text for a preview
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
        if entry is None and "type" in d and "timestamp" in d:
            entry = d
        if not preview and d.get("type") == "assistant":
            msg = d.get("message") or {}
            content = msg.get("content") or []
            for block in content:
                if isinstance(block, dict) and block.get("type") == "text" \
                        and block.get("text", "").strip():
                    preview = block["text"].strip().replace("\n", " ") \
                        .replace("\r", " ")[:100]
                    break
        if entry is not None and preview:
            break
    if entry is None:
        return None
    tool_running = False
    if entry.get("type") == "assistant":
        content = (entry.get("message") or {}).get("content") or []
        if isinstance(content, list):
            tool_running = any(
                isinstance(b, dict) and b.get("type") == "tool_use"
                for b in content)
    return {
        "type": entry.get("type", ""),
        "timestamp": entry.get("timestamp", ""),
        "session_id": session_id or "",
        "preview": preview,
        "tool_running": tool_running,
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
            "transcript_live": False,
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
                ts = parse_ts(le["timestamp"])
                if ts is not None:
                    info["idle_sec"] = max(
                        0, int(time_now.timestamp() - ts))
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
