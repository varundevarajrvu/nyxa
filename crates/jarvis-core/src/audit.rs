//! Append-only JSONL audit log at %APPDATA%\jarvis\audit.jsonl.
//! One line per handled utterance — INCLUDING refusals, kills, and errors
//! (DESIGN.md §5). The `model` field is what lets us verify the §4 routing
//! policy empirically (null = stage-0, no LLM consulted).

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Serialize)]
pub struct Entry<'a> {
    /// RFC 3339 local time.
    pub ts: String,
    pub transcript: &'a str,
    /// "grammar" (stage-0) | "none" (no parser reached a verdict) — LLM
    /// stages add "sonnet"/"opus" in J4.
    pub stage: &'a str,
    /// Which LLM was consulted, if any. Null until J4.
    pub model: Option<&'a str>,
    pub action: Option<&'a str>,
    pub tier: Option<&'a str>,
    pub params: serde_json::Value,
    /// executed | dry_run | needs_confirm_blocked | deny_blocked | disabled |
    /// cooldown | unrecognized | relisten | halted | resumed | exec_error
    pub outcome: &'a str,
    pub latency_ms: u64,
}

pub struct Audit {
    path: PathBuf,
}

impl Audit {
    /// %APPDATA%\jarvis\audit.jsonl (dir created on first use).
    pub fn open_default() -> Result<Self> {
        let base = std::env::var("APPDATA").context("APPDATA not set")?;
        let dir = PathBuf::from(base).join("jarvis");
        std::fs::create_dir_all(&dir).context("create %APPDATA%\\jarvis")?;
        Ok(Self { path: dir.join("audit.jsonl") })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Best-effort append — auditing must never take the pipeline down, but
    /// failures are loudly reported on stderr.
    pub fn log(&self, entry: &Entry) {
        let line = match serde_json::to_string(entry) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[audit] serialize failed: {e}");
                return;
            }
        };
        let r = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = r {
            eprintln!("[audit] write failed: {e}");
        }
    }
}

/// RFC 3339 local timestamp without pulling in chrono: date math on the Unix
/// epoch plus the local UTC offset queried from Win32.
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let offset_min = local_offset_minutes();
    let local = secs + offset_min as i64 * 60;
    let (y, mo, d, h, mi, s) = civil_from_unix(local);
    let (oh, om) = (offset_min / 60, (offset_min % 60).abs());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{}{:02}:{om:02}",
        if offset_min >= 0 { '+' } else { '-' }, oh.abs())
}

#[cfg(windows)]
fn local_offset_minutes() -> i32 {
    // TIME_ZONE_INFORMATION.Bias is UTC-minus-local in minutes; negate.
    unsafe {
        let mut tzi: winapi::um::timezoneapi::TIME_ZONE_INFORMATION = std::mem::zeroed();
        let rc = winapi::um::timezoneapi::GetTimeZoneInformation(&mut tzi);
        let mut bias = tzi.Bias;
        if rc == 2 {
            bias += tzi.DaylightBias; // TIME_ZONE_ID_DAYLIGHT
        } else {
            bias += tzi.StandardBias;
        }
        -bias
    }
}

/// Days-since-epoch -> civil date (Howard Hinnant's algorithm), plus time.
fn civil_from_unix(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi, s) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_shape() {
        let ts = now_rfc3339();
        // e.g. 2026-07-09T14:31:07+05:30
        assert_eq!(ts.len(), 25, "unexpected: {ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[10..11], "T");
    }

    #[test]
    fn civil_epoch() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1, 0, 0, 0));
        // 2026-07-09 00:00:00 UTC
        assert_eq!(civil_from_unix(1_783_555_200), (2026, 7, 9, 0, 0, 0));
    }
}
