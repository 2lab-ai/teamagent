//! Tracing output routing. Two modes, chosen by the command path:
//!
//! - [`init_plain`] — fmt logs to stderr (every non-server command, and
//!   `llmux server` without a TTY). The pre-existing behavior.
//! - [`init_tui_bridge`] — installs ONLY a channel bridge layer: each event
//!   is formatted to one compact line and `try_send`-ed into a bounded
//!   channel the TUI drains into its log console. Nothing writes to
//!   stdout/stderr in this mode — ratatui is the sole owner of the terminal.
//!
//! Both modes respect `RUST_LOG` (default `info`).

use std::fmt::Write as _;

use tokio::sync::mpsc;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt as _};
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::EnvFilter;

/// Bound for the tracing→TUI log channel. The layer drops events on a full
/// channel (best-effort observability — a stalled dashboard must never block
/// or backpressure the code that happened to emit a log line).
pub const LOG_CHANNEL_CAPACITY: usize = 512;

/// One formatted tracing event, ready for the TUI log console.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub level: Level,
    /// `target: message field=value …` — single line, no ANSI escapes.
    pub text: String,
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))
}

/// Plain mode: fmt logs to stderr, `RUST_LOG`-filtered.
pub fn init_plain() {
    tracing_subscriber::fmt()
        .with_env_filter(env_filter())
        .with_writer(std::io::stderr)
        .init();
}

/// TUI mode: install the channel bridge as the only output layer and return
/// the receiver to hand to `tui::run_with`.
pub fn init_tui_bridge() -> mpsc::Receiver<LogLine> {
    let (tx, rx) = mpsc::channel(LOG_CHANNEL_CAPACITY);
    tracing_subscriber::registry()
        .with(env_filter())
        .with(ChannelLayer::new(tx))
        .init();
    rx
}

/// A `tracing_subscriber::Layer` that formats each event to one [`LogLine`]
/// and `try_send`s it. Errors (full or closed channel) are silently dropped:
/// this runs inside arbitrary logging call sites and must never block,
/// panic, or write to the terminal.
pub struct ChannelLayer {
    tx: mpsc::Sender<LogLine>,
}

impl ChannelLayer {
    pub fn new(tx: mpsc::Sender<LogLine>) -> Self {
        Self { tx }
    }
}

impl<S: Subscriber> Layer<S> for ChannelLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);
        let text = format!("{}: {}{}", meta.target(), visitor.message, visitor.fields);
        let _ = self.tx.try_send(LogLine {
            level: *meta.level(),
            text,
        });
    }
}

/// Collects the `message` field and renders every other field as ` k=v`.
#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: String,
}

impl Visit for LineVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscriber(tx: mpsc::Sender<LogLine>) -> impl Subscriber {
        tracing_subscriber::registry().with(ChannelLayer::new(tx))
    }

    #[test]
    fn bridge_formats_level_target_message_and_fields() {
        let (tx, mut rx) = mpsc::channel(8);
        tracing::subscriber::with_default(subscriber(tx), || {
            tracing::warn!(target: "bridge_test", port = 3499, name = %"alpha", "server up");
        });
        let line = rx.try_recv().expect("event delivered");
        assert_eq!(line.level, Level::WARN);
        assert_eq!(line.text, "bridge_test: server up port=3499 name=alpha");
    }

    #[test]
    fn bridge_drops_on_full_channel_without_blocking() {
        let (tx, mut rx) = mpsc::channel(1);
        tracing::subscriber::with_default(subscriber(tx), || {
            tracing::info!("first");
            // Channel is full from here on: these must be dropped, not
            // block the logging call site (try_recv below proves only one
            // event ever landed).
            tracing::info!("second");
            tracing::error!("third");
        });
        let line = rx.try_recv().expect("first event delivered");
        assert!(line.text.ends_with("first"));
        assert!(rx.try_recv().is_err(), "overflow events were dropped");
    }

    #[test]
    fn bridge_survives_closed_channel() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        tracing::subscriber::with_default(subscriber(tx), || {
            tracing::info!("receiver is gone"); // must not panic
        });
    }
}
