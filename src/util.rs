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

/// Strip ANSI escape sequences and stray carriage returns from command output.
///
/// Deploy logs come straight from `git` and `docker`, which emit colour codes
/// and progress-bar redraws. Rendered into HTML those show up as `[0m` litter,
/// so drop them before display.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => {
                // CSI sequences: ESC [ params... final-byte in @..~
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                } else {
                    // Other escapes (e.g. ESC ] ... BEL): drop to the terminator.
                    for next in chars.by_ref() {
                        if next == '\x07' || next == '\n' {
                            break;
                        }
                    }
                }
            }
            // A bare CR is a progress-bar redraw; keep CRLF line endings intact.
            '\r' if chars.peek() != Some(&'\n') => {}
            _ => out.push(ch),
        }
    }
    out
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
