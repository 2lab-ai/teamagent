//! Optional per-request file logging with credential masking (FR1).
//! Disabled by default; never logs raw tokens or API keys.
//!
//! Format (teamclaude-compatible): one file per request named
//! `{yyyymmdd}_{hhmmss}.{ms}_{reqid}.log` (UTC), containing `=== REQUEST ===`
//! / `=== REQUEST BODY ===` / `=== RESPONSE ===` / `=== RESPONSE BODY ===`
//! (first 8 KiB) / `=== ERROR ===` sections. Writes happen on a spawned task,
//! off the request path; a failed write warns and never fails the request.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Cap applied to logged bodies (request and response): first 8 KiB.
pub const BODY_LOG_LIMIT: usize = 8 * 1024;

/// Writes one masked log file per proxied request under `dir`.
#[derive(Debug)]
pub struct RequestLogger {
    dir: PathBuf,
    counter: AtomicU64,
}

impl RequestLogger {
    /// Create the logger, ensuring `dir` exists.
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            counter: AtomicU64::new(0),
        })
    }

    /// Monotonic per-process request id (1-based), used in file names.
    pub fn next_request_id(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Write one request's sections, asynchronously (spawned task — the
    /// request path never waits on disk). Sections are credential-masked
    /// here, uniformly, so no call site can forget.
    pub fn write(self: &Arc<Self>, request_id: u64, sections: Vec<String>) {
        let logger = Arc::clone(self);
        let at = SystemTime::now();
        tokio::spawn(async move {
            let path = logger.dir.join(file_name(at, request_id));
            let content: String = sections
                .iter()
                .map(|s| mask_credentials(s))
                .collect::<Vec<_>>()
                .join("\n\n");
            if let Err(err) = tokio::fs::write(&path, content).await {
                tracing::warn!(path = %path.display(), error = %err, "request log write failed");
            }
        });
    }
}

/// `{yyyymmdd}_{hhmmss}.{ms}_{reqid}.log`, UTC. (teamclaude stamps local
/// time; we use UTC to avoid a timezone dependency — documented deviation.)
pub fn file_name(at: SystemTime, request_id: u64) -> String {
    let elapsed = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = elapsed.as_secs();
    let ms = elapsed.subsec_millis();
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let tod = secs % 86_400;
    format!(
        "{year:04}{month:02}{day:02}_{:02}{:02}{:02}.{ms:03}_{request_id:05}.log",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Days-since-epoch → (year, month, day), Gregorian (Howard Hinnant's
/// `civil_from_days`). Avoids pulling chrono in for one filename.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// Mask every credential-shaped substring before anything reaches disk:
/// `sk-ant-*` tokens keep their first 15 chars, `Bearer <token>` values keep
/// their first 20 chars (teamclaude's truncation widths), `lm-*` proxy keys
/// keep their first 8 — each followed by `...`.
pub fn mask_credentials(text: &str) -> String {
    // (prefix, chars kept from the start of the whole match)
    const RULES: [(&str, usize); 3] = [("Bearer ", 20), ("sk-ant-", 15), ("lm-", 8)];
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    'scan: while i < text.len() {
        let at_token_boundary = i == 0 || !is_token_char(bytes[i - 1]);
        if at_token_boundary {
            for (prefix, keep) in RULES {
                if text[i..].starts_with(prefix) {
                    let mut end = i + prefix.len();
                    while end < bytes.len() && is_token_char(bytes[end]) {
                        end += 1;
                    }
                    let span = &text[i..end];
                    if span.len() > keep {
                        out.push_str(&span[..keep]);
                        out.push_str("...");
                    } else {
                        out.push_str(span);
                    }
                    i = end;
                    continue 'scan;
                }
            }
        }
        // Advance one (possibly multi-byte) char.
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn file_name_formats_utc_timestamp_and_request_id() {
        // 2026-06-12 13:05:09 UTC (verified with `date -u`).
        let at = UNIX_EPOCH + Duration::from_millis(1_781_269_509_042);
        assert_eq!(file_name(at, 7), "20260612_130509.042_00007.log");
    }

    #[test]
    fn request_ids_are_monotonic() {
        let logger = RequestLogger {
            dir: PathBuf::from("/tmp"),
            counter: AtomicU64::new(0),
        };
        assert_eq!(logger.next_request_id(), 1);
        assert_eq!(logger.next_request_id(), 2);
    }

    #[test]
    fn masks_sk_ant_keys_to_fifteen_chars() {
        let input = "x-api-key: sk-ant-api03-AAAABBBBCCCCDDDD";
        assert_eq!(mask_credentials(input), "x-api-key: sk-ant-api03-AA...");
    }

    #[test]
    fn masks_bearer_values_to_twenty_chars() {
        let input = "authorization: Bearer sk-ant-oat01-AAAABBBBCCCC";
        assert_eq!(
            mask_credentials(input),
            "authorization: Bearer sk-ant-oat01-...",
        );
    }

    #[test]
    fn masks_proxy_api_keys() {
        let input = "x-api-key: lm-AAAABBBBCCCCDDDDEEEE";
        assert_eq!(mask_credentials(input), "x-api-key: lm-AAAAB...");
    }

    #[test]
    fn short_values_and_mid_word_matches_are_left_alone() {
        assert_eq!(mask_credentials("sk-ant-x"), "sk-ant-x");
        assert_eq!(mask_credentials("delta-force data"), "delta-force data");
        assert_eq!(mask_credentials("metadata-id"), "metadata-id");
        assert_eq!(mask_credentials("no secrets here"), "no secrets here");
    }

    #[test]
    fn masks_multiple_occurrences_and_keeps_utf8_intact() {
        let input = "한글 sk-ant-api03-AAAABBBBCCCC and sk-ant-oat01-DDDDEEEEFFFF 끝";
        let masked = mask_credentials(input);
        assert_eq!(masked, "한글 sk-ant-api03-AA... and sk-ant-oat01-DD... 끝");
    }

    #[tokio::test]
    async fn write_is_best_effort_and_masked() {
        let dir = std::env::temp_dir().join(format!(
            "llmux-logtest-{}-{}",
            std::process::id(),
            ulid::Ulid::new()
        ));
        let logger = Arc::new(RequestLogger::new(dir.clone()).expect("logger"));
        let id = logger.next_request_id();
        logger.write(
            id,
            vec![
                "=== REQUEST ===\nauthorization: Bearer sk-ant-oat01-SECRETSECRET".to_string(),
                "=== RESPONSE 200 ===".to_string(),
            ],
        );
        // The write is async; poll briefly for the file.
        let mut content = String::new();
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if let Some(entry) = std::fs::read_dir(&dir)
                .ok()
                .and_then(|mut it| it.next())
                .and_then(Result::ok)
            {
                content = std::fs::read_to_string(entry.path()).expect("read log");
                break;
            }
        }
        assert!(
            content.contains("Bearer sk-ant-oat01-..."),
            "masked: {content}"
        );
        assert!(!content.contains("SECRETSECRET"), "no raw token: {content}");
        assert!(content.contains("=== RESPONSE 200 ==="));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
