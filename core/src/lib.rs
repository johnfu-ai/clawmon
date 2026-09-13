//! clawmon-core: platform-independent monitoring logic for Claude Code
//! sessions running inside WSL.
//!
//! Everything except the actual process spawning (`wsl`) is pure logic with
//! unit tests; the Tauri layer in `src-tauri` is a thin shell over this
//! crate. The WSL-side Python scripts (detect.py, usage.py) live beside
//! their Rust adapters here and are driven by this crate's tests.

pub mod detector;
pub mod engine;
pub mod settings;
pub mod usage;
pub mod wsl;

pub use detector::{detect, Detector, RawSession, RawStatus, RawUsage, TmuxInfo};
pub use engine::{
    state_counts, Countdown, Engine, EventKind, Reason, SessionEvent, SessionState, SessionView,
    StatusCounts,
};
pub use settings::Settings;
pub use usage::{parse_usage, query_usage, UsageInfo, UsageLimit};
