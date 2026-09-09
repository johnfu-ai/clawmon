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
from datetime import datetime, timezone

HOME = os.path.expanduser("~")
CLAUDE_DIR = os.path.join(HOME, ".claude", "projects")


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


def slug_for(path):
    return re.sub(r"[^a-zA-Z0-9]", "-", path)


def find_transcript(cwd):
    """Newest transcript jsonl matching the given working directory, or None."""
    if not os.path.isdir(CLAUDE_DIR):
        return None
    # Fast path: the directory name is derived deterministically from cwd.
    d = os.path.join(CLAUDE_DIR, slug_for(cwd))
    candidates = glob.glob(os.path.join(d, "*.jsonl"))
    if candidates:
        return max(candidates, key=os.path.getmtime)
    # Fallback: scan recently modified transcripts for a matching "cwd" field.
    cutoff = 7 * 86400
    all_files = []
    for f in glob.glob(os.path.join(CLAUDE_DIR, "*", "*.jsonl")):
        try:
            m = os.path.getmtime(f)
        except OSError:
            continue
        if m > cutoff:
            all_files.append((m, f))
    all_files.sort(reverse=True)
    for m, f in all_files[:60]:
        try:
            with open(f, "rb") as fh:
                fh.seek(max(0, os.path.getsize(f) - 8192))
                tail = fh.read().decode("utf-8", "replace")
        except OSError:
            continue
        cwds = re.findall(r'"cwd"\s*:\s*"((?:[^"\\]|\\.)*)"', tail)
        if cwds and cwds[-1] == cwd:
            return f
    return None


def json_unescape(s):
    try:
        return json.loads('"' + s + '"')
    except Exception:
        return s


def last_entry(path):
    """Parse the last complete JSON line of the transcript.

    Returns dict with type, timestamp, session_id, preview (may be empty).
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
        if entry is not None and (preview or session_id is None):
            pass
        if entry is not None and preview:
            break
    if entry is None:
        return None
    return {
        "type": entry.get("type", ""),
        "timestamp": entry.get("timestamp", ""),
        "session_id": session_id or "",
        "preview": preview,
    }


def main():
    now = time_now = datetime.now(timezone.utc)
    panes = tmux_panes()
    sessions = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        pid = int(entry)
        cmd = read_cmdline(pid)
        if not is_claude(cmd):
            continue
        st = read_status(pid)
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
        transcript = find_transcript(cwd) if cwd else None
        info = {
            "pid": pid,
            "cwd": cwd,
            "tty": tty,
            "tmux": pane_info,
            "transcript": transcript,
            "session_id": "",
            "last_type": "",
            "last_ts": "",
            "idle_sec": None,
            "preview": "",
        }
        if transcript:
            le = last_entry(transcript)
            if le:
                info["session_id"] = le["session_id"]
                info["last_type"] = le["type"]
                info["last_ts"] = le["timestamp"]
                info["preview"] = le["preview"]
                try:
                    ts = datetime.fromisoformat(
                        le["timestamp"].replace("Z", "+00:00"))
                    if ts.tzinfo is None:
                        ts = ts.replace(tzinfo=timezone.utc)
                    info["idle_sec"] = max(
                        0, int((time_now - ts).total_seconds()))
                except Exception:
                    pass
        sessions.append(info)
    sessions.sort(key=lambda s: s["pid"])
    json.dump({"now": time_now.isoformat(),
               "now_epoch": time_now.timestamp(),
               "sessions": sessions},
              sys.stdout, ensure_ascii=False)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
