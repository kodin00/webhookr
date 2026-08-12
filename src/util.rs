//! Small shared helpers.

use chrono::Utc;
use uuid::Uuid;

/// Current UTC timestamp as an RFC3339 string (second precision).
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Generate a fresh webhook secret (32 hex chars, ~128 bits of entropy).
pub fn generate_secret() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Generate a unique run id.
pub fn new_run_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Constant-time byte comparison (does not short-circuit on length or content).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    let min = a.len().min(b.len());
    for i in 0..min {
        diff |= (a[i] ^ b[i]) as usize;
    }
    diff == 0
}
