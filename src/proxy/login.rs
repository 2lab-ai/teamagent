//! GUI-initiated OAuth login (FR4, `.prd/11-llmux-islands-spec.md`).
//!
//! The existing `/llmux/inject-account` endpoint expects the *client* to run
//! the browser OAuth flow and POST the minted credential. `llmux-islands` wants
//! the opposite: ask the *daemon* to run the flow and add the account itself,
//! so a GUI can add a Claude/Codex subscription without a terminal. This module
//! owns the small state machine that backs `POST /llmux/login/start`,
//! `GET /llmux/login/status`, and `POST /llmux/login/cancel`.
//!
//! Only ONE login is tracked at a time: the OAuth callback binds a fixed
//! localhost port (`auth::oauth::bind_callback_listener` /
//! `CODEX_CALLBACK_PORTS`), so two concurrent logins would contend for it. A
//! second `start` while one is pending is rejected (the endpoint answers 409).
//! No provider token ever passes through this registry — it carries only the
//! opaque `state` id and, on success, the resulting account name.

use std::sync::Mutex;

use tokio::task::JoinHandle;

/// Which provider's browser flow `start` should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginProvider {
    /// Claude (Anthropic) subscription — `auth::oauth` PKCE flow.
    Claude,
    /// ChatGPT (Codex) subscription — `auth::codex` PKCE flow.
    Codex,
}

impl LoginProvider {
    /// Parse the wire `provider` string. Accepts a few friendly aliases so the
    /// app can send either the provider name or the credential `type`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" | "oauth" | "anthropic" => Some(Self::Claude),
            "codex" | "chatgpt" | "openai" => Some(Self::Codex),
            _ => None,
        }
    }

    /// Canonical wire name, echoed back in the `start` response.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }
}

/// Pending-or-terminal state of the one tracked login.
#[derive(Debug, Clone)]
pub enum LoginPhase {
    /// Browser flow is running (or queued) — keep polling.
    Pending,
    /// Completed: the named account was injected into the live pool.
    Done { account: String },
    /// Failed (or cancelled): `message` is a human-readable, token-free reason.
    Error { message: String },
}

impl LoginPhase {
    fn is_pending(&self) -> bool {
        matches!(self, LoginPhase::Pending)
    }
}

struct LoginJob {
    state: String,
    phase: LoginPhase,
    /// Spawn handle, kept only so `cancel` can abort the browser/callback wait.
    /// Cleared once the job reaches a terminal phase.
    handle: Option<JoinHandle<()>>,
}

/// Single-slot login registry held on [`crate::proxy::server::AppState`]. `None`
/// means no login has been started this process; `Some` holds the most recent
/// job (pending or terminal). A terminal job is retained so the poller can read
/// the result, and is replaced on the next `start`.
#[derive(Default)]
pub struct LoginRegistry {
    inner: Mutex<Option<LoginJob>>,
}

impl LoginRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<LoginJob>> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reserve the slot for a new pending login keyed by `state`. Returns
    /// `false` when a login is already pending (the caller answers 409). On
    /// `true`, the caller spawns the flow and then calls [`Self::attach_handle`].
    pub fn begin(&self, state: String) -> bool {
        let mut guard = self.lock();
        if guard.as_ref().is_some_and(|j| j.phase.is_pending()) {
            return false;
        }
        *guard = Some(LoginJob {
            state,
            phase: LoginPhase::Pending,
            handle: None,
        });
        true
    }

    /// Record the spawn handle for the in-flight login. If the slot was already
    /// replaced/cleared (e.g. the task finished or was cancelled before this
    /// ran), the handle is aborted instead of stored.
    pub fn attach_handle(&self, state: &str, handle: JoinHandle<()>) {
        let mut guard = self.lock();
        if let Some(job) = guard.as_mut() {
            if job.state == state && job.phase.is_pending() {
                job.handle = Some(handle);
                return;
            }
        }
        handle.abort();
    }

    /// Record a terminal outcome — only if the slot still holds THIS `state` in
    /// the pending phase, so a `cancel` (or a newer `start`) wins.
    pub fn finish(&self, state: &str, phase: LoginPhase) {
        let mut guard = self.lock();
        if let Some(job) = guard.as_mut() {
            if job.state == state && job.phase.is_pending() {
                job.phase = phase;
                job.handle = None;
            }
        }
    }

    /// Current phase of the login keyed by `state`, or `None` if no job matches
    /// (unknown/expired/replaced).
    pub fn status(&self, state: &str) -> Option<LoginPhase> {
        self.lock()
            .as_ref()
            .filter(|j| j.state == state)
            .map(|j| j.phase.clone())
    }

    /// Cancel the pending login keyed by `state`. Aborts the spawned task and
    /// marks it errored. Returns `true` only if a matching pending job was
    /// cancelled (already-terminal / unknown → `false`).
    pub fn cancel(&self, state: &str) -> bool {
        let mut guard = self.lock();
        if let Some(job) = guard.as_mut() {
            if job.state == state && job.phase.is_pending() {
                if let Some(handle) = job.handle.take() {
                    handle.abort();
                }
                job.phase = LoginPhase::Error {
                    message: "cancelled".into(),
                };
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_parse_accepts_names_and_aliases() {
        assert_eq!(LoginProvider::parse("claude"), Some(LoginProvider::Claude));
        assert_eq!(
            LoginProvider::parse("  CLAUDE "),
            Some(LoginProvider::Claude)
        );
        assert_eq!(
            LoginProvider::parse("anthropic"),
            Some(LoginProvider::Claude)
        );
        assert_eq!(LoginProvider::parse("codex"), Some(LoginProvider::Codex));
        assert_eq!(LoginProvider::parse("chatgpt"), Some(LoginProvider::Codex));
        assert_eq!(LoginProvider::parse("gemini"), None);
        assert_eq!(LoginProvider::parse(""), None);
    }

    #[test]
    fn begin_rejects_a_second_concurrent_login() {
        let reg = LoginRegistry::default();
        assert!(reg.begin("a".into()), "first login starts");
        assert!(
            !reg.begin("b".into()),
            "second login is rejected while the first is pending"
        );
        // The original pending job is untouched by the rejected start.
        assert!(matches!(reg.status("a"), Some(LoginPhase::Pending)));
        assert!(reg.status("b").is_none());
    }

    #[test]
    fn finish_records_terminal_outcome_and_frees_the_slot() {
        let reg = LoginRegistry::default();
        assert!(reg.begin("a".into()));
        reg.finish(
            "a",
            LoginPhase::Done {
                account: "claude:me@x.com".into(),
            },
        );
        match reg.status("a") {
            Some(LoginPhase::Done { account }) => assert_eq!(account, "claude:me@x.com"),
            other => panic!("expected done, got {other:?}"),
        }
        // Slot is free again: a new login may begin.
        assert!(reg.begin("b".into()));
    }

    #[test]
    fn finish_for_a_stale_state_is_ignored() {
        let reg = LoginRegistry::default();
        assert!(reg.begin("a".into()));
        // A late finish carrying the wrong state must not clobber the slot.
        reg.finish(
            "stale",
            LoginPhase::Done {
                account: "x".into(),
            },
        );
        assert!(matches!(reg.status("a"), Some(LoginPhase::Pending)));
    }

    #[test]
    fn cancel_marks_error_and_blocks_a_late_finish() {
        let reg = LoginRegistry::default();
        assert!(reg.begin("a".into()));
        assert!(reg.cancel("a"), "cancel of a pending login succeeds");
        match reg.status("a") {
            Some(LoginPhase::Error { message }) => assert_eq!(message, "cancelled"),
            other => panic!("expected error, got {other:?}"),
        }
        // A finish from the (now aborted) task must not resurrect it.
        reg.finish(
            "a",
            LoginPhase::Done {
                account: "y".into(),
            },
        );
        assert!(matches!(reg.status("a"), Some(LoginPhase::Error { .. })));
        // Cancelling again (already terminal) is a no-op.
        assert!(!reg.cancel("a"));
    }
}
