use crate::settings::Settings;
use crate::wsl::run_wsl_stdin;
use serde::{Deserialize, Serialize};

/// The embedded WSL-side quota query script (same pattern as DETECT_SCRIPT;
/// piped to `wsl.exe -e python3 -` at runtime). It reads the auth token from
/// ~/.claude/settings.json itself — the secret never crosses this boundary.
/// The script lives beside this adapter and is driven by the tests below.
pub const USAGE_SCRIPT: &str = include_str!("usage.py");

/// One quota window (the 5-hour or the weekly credit limit). Field names
/// state their meaning; `RawLimit` below is the adapter that carries the
/// upstream payload's spellings.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimit {
    /// 0-100
    pub percentage: f64,
    /// credits consumed in the window
    pub consumed: u64,
    /// total credits in the window
    pub total: u64,
    pub remaining: u64,
    /// epoch milliseconds
    pub next_reset_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageInfo {
    /// unit == 3 in the quota payload
    pub five_hour: Option<UsageLimit>,
    /// unit == 6 in the quota payload
    pub weekly: Option<UsageLimit>,
    /// plan level, e.g. "pro"
    pub level: String,
}

/// Run the quota query script inside WSL and parse its JSON output.
pub fn query_usage(settings: &Settings) -> Result<UsageInfo, String> {
    let out = run_wsl_stdin(&settings.wsl_distro, &["python3", "-"], USAGE_SCRIPT)?;
    parse_usage(&out)
}

/// Parse the script's one-line reply. Every field defaults, so an older
/// script or a leaner payload still parses (the `transcript_live` pattern).
pub fn parse_usage(raw: &str) -> Result<UsageInfo, String> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Reply {
        error: Option<String>,
        limits: Vec<RawLimit>,
        level: String,
    }
    /// The adapter for the provider's payload: it keeps the upstream field
    /// names (currentValue/usage) so `UsageLimit` above can speak in
    /// intent-revealing names.
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct RawLimit {
        unit: i64,
        percentage: f64,
        #[serde(rename = "currentValue")]
        current_value: u64,
        usage: u64,
        remaining: u64,
        #[serde(rename = "nextResetTime")]
        next_reset_ms: u64,
    }
    let reply: Reply = serde_json::from_str(raw).map_err(|e| format!("解析套餐用量失败: {e}"))?;
    if let Some(err) = reply.error {
        return Err(err);
    }
    let mut info = UsageInfo {
        level: reply.level,
        ..Default::default()
    };
    for l in reply.limits {
        // key on the unit only: the `type` tag has changed server-side
        // before (CREDIT_LIMIT vs TOKENS_LIMIT) while the unit codes held
        let limit = UsageLimit {
            percentage: l.percentage,
            consumed: l.current_value,
            total: l.usage,
            remaining: l.remaining,
            next_reset_ms: l.next_reset_ms,
        };
        match l.unit {
            3 => info.five_hour = Some(limit),
            6 => info.weekly = Some(limit),
            _ => {}
        }
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The payload shape captured live from the quota API on 2026-09-13.
    #[test]
    fn parse_verified_payload() {
        let raw = r#"{"limits":[
            {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":12000,
             "currentValue":2078,"remaining":9921,"percentage":17,
             "nextResetTime":1789299804490},
            {"type":"CREDIT_LIMIT","unit":6,"number":1,"usage":60000,
             "currentValue":13153,"remaining":46846,"percentage":21,
             "nextResetTime":1789726744992}
        ],"level":"pro"}"#;
        let info = parse_usage(raw).unwrap();
        assert_eq!(info.level, "pro");
        let fh = info.five_hour.expect("five-hour window present");
        assert_eq!(fh.percentage, 17.0);
        assert_eq!(fh.consumed, 2078);
        assert_eq!(fh.total, 12000);
        assert_eq!(fh.remaining, 9921);
        assert_eq!(fh.next_reset_ms, 1789299804490);
        let wk = info.weekly.expect("weekly window present");
        assert_eq!(wk.percentage, 21.0);
        assert_eq!(wk.consumed, 13153);
    }

    /// The webview chip reads this shape verbatim (ui/app.js renderUsage and
    /// usageTip). Any rename here must be cross-checked against those
    /// readers — this test is what makes the cross-check mandatory.
    #[test]
    fn usage_info_serializes_the_wire_contract() {
        let info = UsageInfo {
            five_hour: Some(UsageLimit {
                percentage: 17.0,
                consumed: 2078,
                total: 12000,
                remaining: 9921,
                next_reset_ms: 1,
            }),
            weekly: None,
            level: "pro".into(),
        };
        let obj = serde_json::to_value(&info).unwrap();
        let mut keys: Vec<&str> = obj
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        assert_eq!(keys.join(","), "fiveHour,level,weekly");
        let fh = &obj["fiveHour"];
        let mut fk: Vec<&str> = fh.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        fk.sort_unstable();
        assert_eq!(
            fk.join(","),
            "consumed,nextResetMs,percentage,remaining,total"
        );
    }

    #[test]
    fn error_reply_maps_to_err() {
        let info = parse_usage(r#"{"error":"http 401"}"#);
        assert_eq!(info.unwrap_err(), "http 401");
    }

    /// A plan with no quota windows is not an error — the UI just hides
    /// the chip.
    #[test]
    fn missing_limits_is_ok_and_hidden() {
        let info = parse_usage(r#"{"limits":[],"level":"lite"}"#).unwrap();
        assert!(info.five_hour.is_none());
        assert!(info.weekly.is_none());
        assert_eq!(info.level, "lite");
    }

    #[test]
    fn unknown_unit_is_ignored() {
        let raw = r#"{"limits":[
            {"type":"CREDIT_LIMIT","unit":9,"usage":1,"currentValue":1,
             "remaining":0,"percentage":100,"nextResetTime":1}
        ],"level":"pro"}"#;
        let info = parse_usage(raw).unwrap();
        assert!(info.five_hour.is_none());
        assert!(info.weekly.is_none());
    }

    #[test]
    fn malformed_json_is_err() {
        assert!(parse_usage("<html>502</html>").is_err());
    }

    /// Live smoke test (only meaningful inside WSL with a configured plan).
    #[test]
    #[ignore = "queries the real account quota endpoint"]
    fn live_query_usage() {
        let info = query_usage(&Settings::default()).expect("usage query failed");
        println!(
            "5h: {:?} weekly: {:?} level: {}",
            info.five_hour, info.weekly, info.level
        );
    }

    // ---- the real usage.py, driven end to end (Linux only) ---------------
    //
    // The script's own rules are the risky half of this seam: the auth token
    // must never appear in any output, and errors must use the fixed
    // vocabulary. These tests run the actual script against a stub HOME.

    fn write_stub_home(dir: &std::path::Path, base_url: &str, token: &str) {
        let claude = dir.join(".claude");
        std::fs::create_dir_all(&claude).unwrap();
        std::fs::write(
            claude.join("settings.json"),
            format!(r#"{{"env":{{"ANTHROPIC_BASE_URL":"{base_url}","ANTHROPIC_AUTH_TOKEN":"{token}"}}}}"#),
        )
        .unwrap();
    }

    fn run_usage_script(dir: &std::path::Path) -> std::process::Output {
        let script = dir.join("usage.py");
        std::fs::write(&script, USAGE_SCRIPT).unwrap();
        std::process::Command::new("python3")
            .arg(&script)
            .env("HOME", dir)
            // the stub endpoints are loopback-only: a developer proxy in the
            // environment would otherwise answer for them
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("all_proxy")
            .env_remove("ALL_PROXY")
            .output()
            .expect("python3")
    }

    /// The secret rule: an unreachable endpoint yields the fixed-vocabulary
    /// error, and the canary token appears in neither stdout nor stderr.
    #[test]
    #[cfg(target_os = "linux")]
    fn usage_script_error_path_never_leaks_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let canary = format!("canary-token-{}", std::process::id());
        // port 9 (discard) is closed everywhere → network error, not 401
        write_stub_home(tmp.path(), "http://127.0.0.1:9", &canary);
        let out = run_usage_script(tmp.path());
        assert!(out.status.success(), "script itself must not crash");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!combined.contains(&canary), "token leaked: {combined}");
        let err = parse_usage(String::from_utf8_lossy(&out.stdout).trim())
            .expect_err("error replies must parse to Err");
        assert_eq!(err, "network error", "fixed error vocabulary");
    }

    /// The success path: a stub HTTP server stands in for the provider, the
    /// token must ride the Authorization header, and the reply must come
    /// back through `parse_usage` as a `UsageInfo`.
    #[test]
    #[cfg(target_os = "linux")]
    fn usage_script_success_path_parses_through_the_adapter() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let canary = format!("canary-token-{}", std::process::id());
        let token = canary.clone();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("stub server accepts");
            let mut buf = [0u8; 8192];
            let n = sock.read(&mut buf).expect("stub server reads the request");
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = br#"{"data":{"limits":[
                {"type":"CREDIT_LIMIT","unit":3,"number":5,"usage":12000,
                 "currentValue":2078,"remaining":9921,"percentage":17,
                 "nextResetTime":1789299804490}],"level":"pro"}}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(head.as_bytes()).unwrap();
            sock.write_all(body).unwrap();
            req
        });

        let tmp = tempfile::tempdir().unwrap();
        write_stub_home(tmp.path(), &format!("http://{addr}"), &token);
        let out = run_usage_script(tmp.path());
        let req = server.join().expect("stub server served one request");

        assert!(out.status.success());
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!combined.contains(&canary), "token leaked: {combined}");
        assert!(
            req.to_lowercase().contains(&canary.to_lowercase()),
            "the token must travel as the Authorization header, raw (no Bearer)"
        );
        let info = parse_usage(String::from_utf8_lossy(&out.stdout).trim())
            .expect("the script's reply parses through the adapter");
        assert_eq!(info.level, "pro");
        let fh = info.five_hour.expect("five-hour window present");
        assert_eq!((fh.consumed, fh.total), (2078, 12000));
    }
}
