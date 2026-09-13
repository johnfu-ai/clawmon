#!/usr/bin/env python3
"""One-shot GLM Coding Plan quota query, run inside WSL via `python3 -`.

Prints exactly one JSON line to stdout:
  success -> the API's `data` object, e.g. {"limits": [...], "level": "pro"}
  failure -> {"error": "<fixed vocabulary>"} (see main())

Secret rules: the auth token lives only in ~/.claude/settings.json and in
the request header. It must never appear in stdout, stderr, or an error
message — errors use a fixed vocabulary and exception text is never printed.

No third-party dependencies; Python 3.6+ stdlib only.
"""
import json
import os
import sys
import urllib.error
import urllib.request
from urllib.parse import urlsplit, urlunsplit

HOME = os.path.expanduser("~")
# the CLI reads both files for its `env` block, so a token moved to the
# local override by /update-config must still be found
SETTINGS_FILES = (
    os.path.join(HOME, ".claude", "settings.json"),
    os.path.join(HOME, ".claude", "settings.local.json"),
)

QUOTA_PATH = "/api/monitor/usage/quota/limit"


def read_env(name):
    """First hit for `name` across the settings files' env blocks."""
    for path in SETTINGS_FILES:
        try:
            with open(path) as f:
                data = json.load(f)
        except (OSError, ValueError):
            continue
        env = data.get("env")
        if isinstance(env, dict) and isinstance(env.get(name), str):
            return env[name]
    return None


def quota_url(base):
    """`https://api.example.com/api/anthropic` -> scheme://host + QUOTA_PATH."""
    parts = urlsplit(base)
    if parts.scheme not in ("http", "https") or not parts.netloc:
        return None
    return urlunsplit((parts.scheme, parts.netloc, QUOTA_PATH, "", ""))


def main():
    base = read_env("ANTHROPIC_BASE_URL")
    token = read_env("ANTHROPIC_AUTH_TOKEN")
    if not base or not token:
        print(json.dumps({"error": "missing claude config"}))
        return
    url = quota_url(base)
    if not url:
        print(json.dumps({"error": "bad base url"}))
        return
    req = urllib.request.Request(
        url,
        headers={
            # the API wants the raw token, no Bearer prefix
            "Authorization": token,
            "Accept-Language": "en-US,en",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            body = json.loads(resp.read().decode("utf-8", "replace"))
    except urllib.error.HTTPError as e:
        # the code only — the body and headers never carry secrets, but a
        # fixed vocabulary keeps that guaranteed
        print(json.dumps({"error": "http %d" % e.code}))
        return
    except Exception:
        print(json.dumps({"error": "network error"}))
        return
    data = body.get("data") if isinstance(body, dict) else None
    if not isinstance(data, dict):
        print(json.dumps({"error": "bad response"}))
        return
    json.dump(data, sys.stdout)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
