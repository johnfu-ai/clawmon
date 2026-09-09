#!/usr/bin/env python3
"""WSL-side Claude Code session detector.

Runs inside WSL (invoked as `wsl.exe -e python3 -` with this script on stdin).
Prints one JSON object to stdout describing every live Claude Code process.

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


def read_cmdline(pid):
    try:
        with open("/proc/%d/cmdline" % pid, "rb") as f:
            return [t.decode("utf-8", "replace") for t in f.read().split(b"\0") if t]
    except OSError:
        return []


def read_status(pid):
    """Return dict of /proc/<pid>/status key/values (we need PPid)."""
    info = {}
    try:
        with open("/proc/%d/status" % pid) as f:
            for line in f:
                if ":" in line:
                    k, v = line.split(":", 1)
                    info[k] = v.strip()
    except OSError:
        pass
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


def last_entry(path):
    """Parse the last complete JSON line of the transcript.

    Returns dict with type, timestamp, session_id, preview, tool_running.
    `tool_running` is True when the last timestamped entry is an assistant
    message containing a tool_use block — the tool result is only appended
    when the tool finishes, so this is exactly "a tool is executing now".
    """
    try:
        size = os.path.getsize(path)
        with open(path, "rb") as fh:
            fh.seek(max(0, size - 65536))
            tail = fh.read().decode("utf-8", "replace")
    except OSError:
        return None
    lines = [l for l in tail.split("\n") if l.strip()]
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
                    preview = block["text"].strip().replace("\n", " ")[:100]
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


def main():
    now = time_now = datetime.now(timezone.utc)
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
        }
        transcript = info["transcript"]
        if transcript:
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
        sessions.append(info)
    sessions.sort(key=lambda s: s["pid"])
    json.dump({"now": time_now.isoformat(),
               "now_epoch": time_now.timestamp(),
               "sessions": sessions},
              sys.stdout, ensure_ascii=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
