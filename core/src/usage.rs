use crate::settings::Settings;
use crate::wsl::run_wsl_stdin;
use serde::{Deserialize, Serialize};

/// The embedded WSL-side quota query script (same pattern as DETECT_SCRIPT;
/// piped to `wsl.exe -e python3 -` at runtime). It reads the auth token from
/// ~/.claude/settings.json itself — the secret never crosses this boundary.
pub const USAGE_SCRIPT: &str = include_str!("../../src-tauri/src/usage.py");

/// One quota window (e.g. the 5-hour or the monthly credit limit).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimit {
    /// 0-100
    pub percentage: f64,
    /// credits consumed in the window
    pub current_value: u64,
    /// total credits in the window
    pub usage: u64,
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
            current_value: l.current_value,
            usage: l.usage,
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
        assert_eq!(fh.current_value, 2078);
        assert_eq!(fh.usage, 12000);
        assert_eq!(fh.remaining, 9921);
        assert_eq!(fh.next_reset_ms, 1789299804490);
        let wk = info.weekly.expect("weekly window present");
        assert_eq!(wk.percentage, 21.0);
        assert_eq!(wk.current_value, 13153);
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
}
