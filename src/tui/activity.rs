//! Activity log state: in-flight requests (spinner rows), a bounded ring
//! buffer of completed entries (newest first), and per-account totals.
//! Pure state — rendering lives in `ui`, timestamps are passed in.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::event::{ActivityEvent, TokenCounts};

/// Completed-entry ring capacity (matches teamclaude's 200-line log).
pub(crate) const LOG_CAPACITY: usize = 200;
/// Distinct-client cap for per-client request attribution (issue #32). The
/// `metadata.user_id` space is operator/agent controlled and small in practice,
/// but a hostile or buggy client could send a fresh id per request; this bounds
/// the in-memory map so it can never grow unbounded. When the cap is reached, a
/// *new* client id is folded into the shared `unknown` bucket instead of
/// allocating a new entry (existing ids keep accumulating). The `unknown`
/// bucket itself never counts against the cap.
pub(crate) const MAX_CLIENTS: usize = 1024;
/// The bucket name for requests with no `metadata.user_id` (issue #32). These
/// are attributed here, never dropped.
pub(crate) const UNKNOWN_CLIENT: &str = "unknown";
/// In-flight rows are bounded too: if the proxy never sends a finish (bug or
/// dropped event), the oldest in-flight entry is retired as an error note
/// instead of leaking forever.
const MAX_IN_FLIGHT: usize = 64;
/// Age after which an in-flight row is presumed finished and swept, even if no
/// `RequestFinished` event ever arrived (the event was dropped on a full
/// activity channel). Real requests finish in well under 90s per the daemon
/// logs, so 300s is a wide safety margin that never retires a live request but
/// still bounds a leaked row's lifetime — instead of growing to 25,000s+.
const STALE_IN_FLIGHT: Duration = Duration::from_secs(300);

/// A request that has started but not finished — rendered with a spinner.
/// `group`/`model` are filled at routing time so the dashboard can attribute
/// in-flight requests to a model row before they complete (req11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InFlight {
    pub id: u64,
    pub method: String,
    pub path: String,
    pub account: Option<String>,
    pub group: Option<String>,
    pub model: Option<String>,
    pub started_at: SystemTime,
}

/// Body of a completed log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CompletedBody {
    Request {
        method: String,
        path: String,
        account: Option<String>,
        status: u16,
        duration: Duration,
        tokens: Option<TokenCounts>,
        /// Backend group ("claude"/"codex"), model slug, and reasoning effort
        /// served for this request, when known.
        group: Option<String>,
        model: Option<String>,
        effort: Option<String>,
    },
    Note {
        text: String,
        error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Completed {
    pub at: SystemTime,
    pub body: CompletedBody,
}

/// A STABLE identity for a completed *request* entry, used by the TUI to track
/// which activity row is click-expanded across redraws (Feature B). The
/// completed-entry body carries no request `id` (it is dropped at finish), so
/// the key is the tuple that survives new rows prepending: completion time
/// (epoch ms) + method + path + status. A list index would NOT survive (new
/// rows shift everything down), so we never key on position. `Note` entries are
/// not expandable and have no key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ActivityKey {
    pub at_ms: u64,
    pub method: String,
    pub path: String,
    pub status: u16,
}

impl Completed {
    /// Stable expand-identity for this entry, or `None` when it is a `Note`
    /// (notes are never expandable — they carry no request detail).
    pub(crate) fn activity_key(&self) -> Option<ActivityKey> {
        match &self.body {
            CompletedBody::Request {
                method,
                path,
                status,
                ..
            } => Some(ActivityKey {
                at_ms: self
                    .at
                    .duration_since(UNIX_EPOCH)
                    .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
                    .unwrap_or(0),
                method: method.clone(),
                path: path.clone(),
                status: *status,
            }),
            CompletedBody::Note { .. } => None,
        }
    }
}

/// Per-account lifetime counters for the table's totals columns and the
/// global totals pane (ok/error split + in/out token split).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub requests: u64,
    /// Requests that finished with status < 400.
    pub ok: u64,
    /// Requests that finished with status >= 400.
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl Totals {
    /// Combined token count for single-number columns.
    pub(crate) fn tokens(&self) -> u64 {
        self.tokens_in.saturating_add(self.tokens_out)
    }

    fn add(&mut self, other: &Totals) {
        self.requests = self.requests.saturating_add(other.requests);
        self.ok = self.ok.saturating_add(other.ok);
        self.errors = self.errors.saturating_add(other.errors);
        self.tokens_in = self.tokens_in.saturating_add(other.tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(other.tokens_out);
    }
}

// ---------------------------------------------------------------------------
// Model-usage aggregation (req1-20): per (group, served_model) row.
// ---------------------------------------------------------------------------

/// In-memory accumulator for one model row. Folded from completed request
/// events; reset on daemon restart (runtime-only, req26). Cache counters are
/// optional — `None` until an upstream reports the field (req8/9).
#[derive(Debug, Default, Clone)]
struct ModelStats {
    requests: u64,
    ok: u64,
    errors: u64,
    tokens_in: u64,
    tokens_out: u64,
    cache_read: Option<u64>,
    cache_creation: Option<u64>,
    last_used: Option<SystemTime>,
    /// Which account(s) served this model (req19).
    accounts: HashMap<String, Totals>,
    /// Reasoning/effort label → request count (req18); `"none"` when unset.
    efforts: HashMap<String, u64>,
    /// Endpoint class → request count (req20): `messages`/`count_tokens`/other.
    endpoints: HashMap<String, u64>,
}

/// A finished aggregated model row (snapshot of [`ModelStats`]). Timestamps are
/// kept as `SystemTime`; the document builder converts to epoch ms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelUsage {
    pub group: String,
    pub model: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: Option<u64>,
    pub cache_creation: Option<u64>,
    pub last_used: SystemTime,
    pub accounts: Vec<ModelAccount>,
    pub efforts: Vec<ModelCount>,
    pub endpoints: Vec<ModelCount>,
}

/// Per-account contribution to one model row (req19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelAccount {
    pub name: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

/// A labelled request count (effort level or endpoint class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelCount {
    pub label: String,
    pub requests: u64,
}

/// Strip a trailing display-only context suffix `…[1m]` so usage is not split
/// by client window hints (req17): `claude-sonnet-4-5[1m]` → `claude-sonnet-4-5`.
pub(crate) fn normalize_model(model: &str) -> String {
    match model.split_once('[') {
        Some((base, _)) => base.trim().to_string(),
        None => model.trim().to_string(),
    }
}

/// Classify a request path into an endpoint bucket for the per-model breakdown
/// (req20). `count_tokens` is checked first because its path also contains
/// `/messages`.
fn endpoint_class(path: &str) -> String {
    let p = path.split('?').next().unwrap_or(path);
    if p.contains("count_tokens") {
        "count_tokens".to_string()
    } else if p.contains("/messages") {
        "messages".to_string()
    } else {
        p.rsplit('/')
            .find(|s| !s.is_empty())
            .unwrap_or("other")
            .to_string()
    }
}

fn sorted_counts(map: &HashMap<String, u64>) -> Vec<ModelCount> {
    let mut counts: Vec<ModelCount> = map
        .iter()
        .map(|(label, &requests)| ModelCount {
            label: label.clone(),
            requests,
        })
        .collect();
    counts.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.label.cmp(&b.label)));
    counts
}

// ---------------------------------------------------------------------------
// Windowed bucket ring (issue #23): rolling hourly counters keyed by
// (group, normalized_model, account), so 24h / 72h per-account/per-model
// heatmaps are computable IN MEMORY (no durable store — that is a follow-up).
//
// Each bucket covers one wall-clock hour (epoch-hour index = secs / 3600). The
// ring keeps [`BUCKET_COUNT`] hours; folding a request rolls the ring forward
// to the current hour and PRUNES expired buckets entirely (not just zeroes
// them) so stray/typo model keys can never grow unbounded. SystemTime is not
// monotonic — every hour computation clamps on skew rather than panicking.
// ---------------------------------------------------------------------------

/// Seconds per bucket — one wall-clock hour.
const BUCKET_SECS: u64 = 3600;
/// How many hourly buckets the ring retains. 73 covers a full 72h window plus
/// the current partial hour, so a 72h view never loses a still-relevant hour
/// to roll-forward before the window itself expires it.
const BUCKET_COUNT: usize = 73;

/// The windows the heatmap surfaces. Kept small and fixed (24h, 72h) — both fit
/// inside the retained ring, so each is exact up to the lossy event channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum StatsWindow {
    #[default]
    Day,
    ThreeDay,
}

impl StatsWindow {
    /// All windows the dashboard renders, narrowest first.
    pub(crate) const ALL: [StatsWindow; 2] = [StatsWindow::Day, StatsWindow::ThreeDay];

    /// The next window in the cycle (24h ↔ 72h), for the `w` toggle.
    pub(crate) fn next(self) -> StatsWindow {
        match self {
            StatsWindow::Day => StatsWindow::ThreeDay,
            StatsWindow::ThreeDay => StatsWindow::Day,
        }
    }

    /// Trailing duration this window aggregates over.
    pub(crate) fn duration(self) -> Duration {
        match self {
            StatsWindow::Day => Duration::from_secs(24 * 3600),
            StatsWindow::ThreeDay => Duration::from_secs(72 * 3600),
        }
    }

    /// Short label for the UI ("24h" / "72h").
    pub(crate) fn label(self) -> &'static str {
        match self {
            StatsWindow::Day => "24h",
            StatsWindow::ThreeDay => "72h",
        }
    }
}

/// Per-bucket counters for one `(group, model, account)` key. Mirrors the
/// cumulative `ModelStats` fields the issue calls for, but scoped to one hour.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WindowCounts {
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl WindowCounts {
    fn add(&mut self, other: &WindowCounts) {
        self.requests = self.requests.saturating_add(other.requests);
        self.ok = self.ok.saturating_add(other.ok);
        self.errors = self.errors.saturating_add(other.errors);
        self.tokens_in = self.tokens_in.saturating_add(other.tokens_in);
        self.tokens_out = self.tokens_out.saturating_add(other.tokens_out);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_creation = self.cache_creation.saturating_add(other.cache_creation);
    }

    /// Combined token count for the heatmap intensity (in + out + cache).
    pub(crate) fn tokens(&self) -> u64 {
        self.tokens_in
            .saturating_add(self.tokens_out)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
    }
}

/// The bucket key. Carries `group` AND `model` AND `account` so a same-named
/// model under two providers stays two rows and the per-account axis exists.
type WindowKey = (String, String, String);

/// One hour's counters: the epoch-hour index it represents + the per-key map.
#[derive(Debug, Default, Clone)]
struct Bucket {
    /// `epoch_secs / BUCKET_SECS`. A bucket is "current" when this equals the
    /// hour derived from `now`.
    hour: u64,
    counts: HashMap<WindowKey, WindowCounts>,
}

/// A fixed-capacity ring of hourly buckets. Folding is O(1) amortized: roll
/// forward to the current hour (reusing slots, clearing stale ones) then bump
/// the one current bucket's key.
#[derive(Debug, Default)]
struct WindowedBuckets {
    buckets: VecDeque<Bucket>,
}

/// Epoch-hour index for `now`, clamped on a pre-epoch clock (skew defence —
/// never panics).
fn epoch_hour(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() / BUCKET_SECS)
        .unwrap_or(0)
}

impl WindowedBuckets {
    /// Roll the ring forward so its newest bucket is `current_hour`, dropping
    /// (pruning, not zeroing) buckets that fall out of the retained range. A
    /// backwards clock (`current_hour` older than the newest) is ignored — we
    /// never rewind, so skew can't corrupt or panic the ring.
    fn roll_to(&mut self, current_hour: u64) {
        match self.buckets.back() {
            Some(newest) if current_hour <= newest.hour => return,
            _ => {}
        }
        // Append the current hour. If the ring already has buckets and there is
        // a gap (idle hours), we do NOT materialize the empty intermediate
        // hours — pruning by `hour` value at read time handles the window math,
        // and a single appended current bucket keeps roll-forward O(1).
        self.buckets.push_back(Bucket {
            hour: current_hour,
            counts: HashMap::new(),
        });
        // Prune anything older than the retained range AND cap the deque length
        // (an idle-then-active daemon can leave sparse old buckets).
        let oldest_kept = current_hour.saturating_sub(BUCKET_COUNT as u64 - 1);
        self.buckets.retain(|b| b.hour >= oldest_kept);
        while self.buckets.len() > BUCKET_COUNT {
            self.buckets.pop_front();
        }
    }

    /// Fold one finished, attributed request into the current bucket.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        group: &str,
        model: &str,
        account: &str,
        status: u16,
        tokens: Option<TokenCounts>,
        now: SystemTime,
    ) {
        let hour = epoch_hour(now);
        self.roll_to(hour);
        // After roll_to, the matching current bucket is the newest with
        // `hour == current`; if a backwards clock skipped the append, fold into
        // the newest bucket we have rather than dropping the event.
        let bucket = match self.buckets.back_mut() {
            Some(b) => b,
            None => {
                self.buckets.push_back(Bucket {
                    hour,
                    counts: HashMap::new(),
                });
                self.buckets.back_mut().expect("just pushed")
            }
        };
        let key = (
            group.to_string(),
            normalize_model(model),
            account.to_string(),
        );
        let entry = bucket.counts.entry(key).or_default();
        entry.requests = entry.requests.saturating_add(1);
        if status < 400 {
            entry.ok = entry.ok.saturating_add(1);
        } else {
            entry.errors = entry.errors.saturating_add(1);
        }
        if let Some(t) = tokens {
            entry.tokens_in = entry.tokens_in.saturating_add(t.input);
            entry.tokens_out = entry.tokens_out.saturating_add(t.output);
            entry.cache_read = entry.cache_read.saturating_add(t.cache_read.unwrap_or(0));
            entry.cache_creation = entry
                .cache_creation
                .saturating_add(t.cache_creation.unwrap_or(0));
        }
    }

    /// Aggregate every key over the trailing `window` ending at `now`, summing
    /// the buckets whose hour falls inside it. Returns one [`WindowedRow`] per
    /// `(group, model, account)` with any activity in the window.
    fn aggregate(&self, window: StatsWindow, now: SystemTime) -> Vec<WindowedRow> {
        let current_hour = epoch_hour(now);
        // Number of whole hours the window spans; the trailing bucket is the
        // current hour, so a 24h window includes the current hour + 23 prior.
        let span_hours = (window.duration().as_secs() / BUCKET_SECS).max(1);
        let cutoff_hour = current_hour.saturating_sub(span_hours - 1);
        let mut acc: HashMap<WindowKey, WindowCounts> = HashMap::new();
        for bucket in &self.buckets {
            if bucket.hour < cutoff_hour || bucket.hour > current_hour {
                continue;
            }
            for (key, counts) in &bucket.counts {
                acc.entry(key.clone()).or_default().add(counts);
            }
        }
        let mut rows: Vec<WindowedRow> = acc
            .into_iter()
            .map(|((group, model, account), counts)| WindowedRow {
                group,
                model,
                account,
                counts,
            })
            .collect();
        // Deterministic order: tokens desc, then key — the heatmap reads top-down.
        rows.sort_by(|a, b| {
            b.counts
                .tokens()
                .cmp(&a.counts.tokens())
                .then(b.counts.requests.cmp(&a.counts.requests))
                .then(a.group.cmp(&b.group))
                .then(a.model.cmp(&b.model))
                .then(a.account.cmp(&b.account))
        });
        rows
    }
}

/// One aggregated windowed cell: a `(group, model, account)` triple and its
/// summed counters over a window. The heatmap renders one of these per visible
/// cell; the document carries the full set per window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowedRow {
    pub group: String,
    pub model: String,
    pub account: String,
    pub counts: WindowCounts,
}

// ---------------------------------------------------------------------------
// Persistence (req-persist): append-only JSONL of finished requests.
//
// Two user requirements satisfied by ONE store: (A) model/account stats survive
// restart and continue cumulatively, and (C) activity request/response records
// are persisted with no retention limit. The single source of truth is one
// JSON line per `RequestFinished`, replayed on startup through the SAME `apply`
// fold so the rebuilt aggregates are bit-for-bit identical to the live ones —
// no double-counting. Mirrors `proxy::codex_trace`: best-effort append, every
// IO/serde error swallowed, the request path is NEVER affected.
// ---------------------------------------------------------------------------

/// On-disk schema version for [`PersistedRequest`]. Bumped only on a
/// breaking layout change; older/garbage lines are skipped on load, never
/// fatal.
const PERSIST_VERSION: u8 = 1;

/// One finished request, serialized as a single JSON line. Carries exactly the
/// fields of an [`ActivityEvent::RequestFinished`] needed to reconstruct it for
/// replay (`Duration` flattened to `duration_ms`, `SystemTime` to `ts_ms` since
/// the Unix epoch). Field-named JSON so adding a field stays backward-readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PersistedRequest {
    /// Schema version (`= PERSIST_VERSION`). Lines with an unknown version are
    /// skipped on load.
    pub v: u8,
    /// Completion timestamp, millis since the Unix epoch.
    pub ts_ms: u64,
    pub id: u64,
    pub method: String,
    pub path: String,
    pub account: Option<String>,
    pub status: u16,
    pub duration_ms: u64,
    pub tokens: Option<TokenCounts>,
    pub group: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Keyless per-client metering identity (issue #32). Additive: lines
    /// persisted before this field default to `None` and replay into the
    /// `unknown` client bucket.
    #[serde(default)]
    pub user_id: Option<String>,
}

impl PersistedRequest {
    /// Build a record from a `RequestFinished` event's fields + the `now` it was
    /// folded at. Returns `None` for any other event variant (only finished
    /// requests are persisted — notes/switches/polls are runtime-only).
    pub(crate) fn from_event(event: &ActivityEvent, now: SystemTime) -> Option<Self> {
        let ActivityEvent::RequestFinished {
            id,
            method,
            path,
            account,
            status,
            duration,
            tokens,
            group,
            model,
            effort,
            user_id,
        } = event
        else {
            return None;
        };
        let ts_ms = now
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        Some(Self {
            v: PERSIST_VERSION,
            ts_ms,
            id: *id,
            method: method.clone(),
            path: path.clone(),
            account: account.clone(),
            status: *status,
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            tokens: *tokens,
            group: group.clone(),
            model: model.clone(),
            effort: effort.clone(),
            user_id: user_id.clone(),
        })
    }

    /// Reconstruct the `(event, ts)` pair this record was built from, so replay
    /// can fold it through `ActivityLog::apply` exactly as the live event was.
    fn into_event(self) -> (ActivityEvent, SystemTime) {
        let ts = UNIX_EPOCH + Duration::from_millis(self.ts_ms);
        let event = ActivityEvent::RequestFinished {
            id: self.id,
            method: self.method,
            path: self.path,
            account: self.account,
            status: self.status,
            duration: Duration::from_millis(self.duration_ms),
            tokens: self.tokens,
            group: self.group,
            model: self.model,
            effort: self.effort,
            user_id: self.user_id,
        };
        (event, ts)
    }
}

/// Append one finished-request record to `path` as a single JSON line,
/// best-effort. A `None` path (no state dir), a non-`RequestFinished` event, or
/// any IO/serde error is swallowed — exactly like [`crate::proxy::codex_trace`]
/// — so persistence can never break or slow the request path. The parent dir is
/// created if missing; the file is opened `create(true).append(true)`.
pub(crate) fn persist_request(path: Option<&Path>, event: &ActivityEvent, now: SystemTime) {
    let Some(path) = path else {
        return;
    };
    let Some(record) = PersistedRequest::from_event(event, now) else {
        return;
    };
    let Ok(line) = serde_json::to_string(&record) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = writeln!(file, "{line}");
}

#[derive(Debug, Default)]
pub(crate) struct ActivityLog {
    capacity: usize,
    in_flight: Vec<InFlight>,
    /// Front = newest (the log renders newest-top).
    completed: VecDeque<Completed>,
    totals: HashMap<String, Totals>,
    /// Requests that finished before routing (no account) — kept out of the
    /// per-account map but included in the global totals.
    unrouted: Totals,
    /// Per (group, served_model) usage rows (req1-20). Keyed by the normalized
    /// served model within its backend group.
    models: HashMap<(String, String), ModelStats>,
    /// Per-client request attribution (issue #32), keyed by `metadata.user_id`
    /// (the `unknown` bucket holds requests with no id). In-memory only —
    /// runtime accounting, reset on restart, never persisted to disk. Bounded
    /// by [`MAX_CLIENTS`] distinct ids (the `unknown` bucket excluded). This is
    /// pure metering: counting requests/tokens per client, never gating.
    clients: HashMap<String, Totals>,
    /// Rolling hourly bucket ring for the windowed (24h/72h) per-account
    /// per-model heatmap (issue #23). In-memory only — durable persistence is a
    /// follow-up. Keyed by (group, normalized_model, account).
    windowed: WindowedBuckets,
}

/// A finished per-client attribution row (issue #32): one client identity
/// (`metadata.user_id`, or `unknown`) and its lifetime request/token counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientUsage {
    pub client: String,
    pub requests: u64,
    pub ok: u64,
    pub errors: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl ActivityLog {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ..Self::default()
        }
    }

    /// Replay a persisted activity log (req-persist A/C): read `path`
    /// line-by-line and fold every parseable [`PersistedRequest`] back through
    /// [`Self::apply`] at its original timestamp, rebuilding the cumulative
    /// model/account aggregates and seeding the activity ring. Same fold as the
    /// live path → the restored math is identical, no double-counting.
    ///
    /// Best-effort and total: a `None` path or missing file is a no-op; a line
    /// that is not valid JSON, or whose `v` is not the current
    /// [`PERSIST_VERSION`], is skipped (tolerating corruption and old formats);
    /// nothing here panics. The ring's capacity still bounds the in-memory
    /// `completed` view — replaying a huge log keeps the totals but only the
    /// newest `capacity` request lines stay visible (req C keeps the FILE
    /// complete; the ring is the display window).
    pub(crate) fn load_persisted(&mut self, path: Option<&Path>) {
        let Some(path) = path else {
            return;
        };
        let Ok(contents) = std::fs::read_to_string(path) else {
            // Missing file (or unreadable) = nothing to resume from.
            return;
        };
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<PersistedRequest>(line) else {
                continue; // corrupt / not a PersistedRequest line
            };
            if record.v != PERSIST_VERSION {
                continue; // older/newer schema — skip rather than misread
            }
            let (event, ts) = record.into_event();
            self.apply(event, ts);
        }
    }

    pub(crate) fn in_flight(&self) -> &[InFlight] {
        &self.in_flight
    }

    /// Sweep in-flight rows older than [`STALE_IN_FLIGHT`]: their
    /// `RequestFinished` event was almost certainly dropped on a full activity
    /// channel (the daemon reports the request as completed while the dashboard
    /// would otherwise show it pinned forever). Each swept row leaves a note so
    /// the cause is visible in the log rather than silently vanishing.
    ///
    /// Called on every dashboard read (`view`) and at the top of `apply` so a
    /// leaked row is bounded even with no further activity. Idempotent and
    /// cheap (a single retain over a ≤64-entry vec).
    pub(crate) fn prune_stale_in_flight(&mut self, now: SystemTime) {
        let mut swept: Vec<InFlight> = Vec::new();
        self.in_flight.retain(|entry| {
            let stale = now
                .duration_since(entry.started_at)
                .map(|age| age >= STALE_IN_FLIGHT)
                .unwrap_or(false);
            if stale {
                swept.push(entry.clone());
            }
            !stale
        });
        for entry in swept {
            self.push_note(
                format!(
                    "{} {} presumed finished (activity event dropped)",
                    entry.method, entry.path
                ),
                true,
                now,
            );
        }
    }

    /// Completed entries, newest first.
    pub(crate) fn completed(&self) -> impl Iterator<Item = &Completed> {
        self.completed.iter()
    }

    /// Per-account totals lookup. The dashboard reads the whole map
    /// ([`Self::totals_map`]) for the document; this single-account accessor
    /// is exercised by the unit tests.
    #[cfg(test)]
    pub(crate) fn totals_for(&self, account: &str) -> Totals {
        self.totals.get(account).copied().unwrap_or_default()
    }

    /// Clone of the per-account totals map (the dashboard document carries
    /// every account's session totals, not just the ones on screen).
    pub(crate) fn totals_map(&self) -> HashMap<String, Totals> {
        self.totals.clone()
    }

    /// Lifetime totals across every account, unrouted failures included.
    pub(crate) fn totals_global(&self) -> Totals {
        let mut sum = self.unrouted;
        for totals in self.totals.values() {
            sum.add(totals);
        }
        sum
    }

    /// Fold one attributed completed request into its `(group, model)` row.
    #[allow(clippy::too_many_arguments)]
    fn record_model(
        &mut self,
        group: &str,
        model: &str,
        account: &Option<String>,
        status: u16,
        tokens: Option<TokenCounts>,
        effort: &Option<String>,
        path: &str,
        now: SystemTime,
    ) {
        let entry = self
            .models
            .entry((group.to_string(), normalize_model(model)))
            .or_default();
        entry.requests += 1;
        if status < 400 {
            entry.ok += 1;
        } else {
            entry.errors += 1;
        }
        if let Some(t) = tokens {
            entry.tokens_in = entry.tokens_in.saturating_add(t.input);
            entry.tokens_out = entry.tokens_out.saturating_add(t.output);
            entry.cache_read = crate::proxy::sse::add_opt(entry.cache_read, t.cache_read);
            entry.cache_creation =
                crate::proxy::sse::add_opt(entry.cache_creation, t.cache_creation);
        }
        entry.last_used = Some(now);
        let effort_label = effort.clone().unwrap_or_else(|| "none".to_string());
        *entry.efforts.entry(effort_label).or_default() += 1;
        *entry.endpoints.entry(endpoint_class(path)).or_default() += 1;
        if let Some(name) = account {
            let at = entry.accounts.entry(name.clone()).or_default();
            at.requests += 1;
            if status < 400 {
                at.ok += 1;
            } else {
                at.errors += 1;
            }
            if let Some(t) = tokens {
                at.tokens_in = at.tokens_in.saturating_add(t.input);
                at.tokens_out = at.tokens_out.saturating_add(t.output);
            }
            // Fold the same request into the windowed bucket ring (issue #23).
            // Only account-attributed requests get a per-account cell; the key
            // carries group AND normalized model AND account so providers and
            // accounts never merge. `group`/`model` are normalized inside.
            self.windowed
                .record(group, model, name, status, tokens, now);
        }
    }

    /// Fold one finished request into its per-client bucket (issue #32).
    /// `user_id` is the `metadata.user_id` (or `None` → the `unknown` bucket).
    /// Bounded by [`MAX_CLIENTS`]: once that many distinct ids are tracked, a
    /// brand-new id is merged into `unknown` rather than allocating a new
    /// entry (already-tracked ids and `unknown` always accumulate). This is
    /// counting only — it never affects whether the request was served.
    fn record_client(&mut self, user_id: Option<&str>, status: u16, tokens: Option<TokenCounts>) {
        let key = match user_id {
            Some(id) if !id.is_empty() => {
                // Cap distinct named clients: an unseen id past the cap is
                // folded into `unknown` so the map cannot grow unbounded.
                if self.clients.contains_key(id) || self.clients.len() < MAX_CLIENTS {
                    id.to_string()
                } else {
                    UNKNOWN_CLIENT.to_string()
                }
            }
            _ => UNKNOWN_CLIENT.to_string(),
        };
        let bucket = self.clients.entry(key).or_default();
        bucket.requests += 1;
        if status < 400 {
            bucket.ok += 1;
        } else {
            bucket.errors += 1;
        }
        if let Some(t) = tokens {
            bucket.tokens_in = bucket.tokens_in.saturating_add(t.input);
            bucket.tokens_out = bucket.tokens_out.saturating_add(t.output);
        }
    }

    /// Per-client attribution lookup (issue #32), exercised by the tests.
    #[cfg(test)]
    pub(crate) fn client_totals(&self, client: &str) -> Totals {
        self.clients.get(client).copied().unwrap_or_default()
    }

    /// Snapshot of every per-client attribution row (issue #32), sorted by
    /// requests desc, then total tokens desc, then client name. The `unknown`
    /// bucket sorts by the same key as any other (it is just another client).
    pub(crate) fn client_usage(&self) -> Vec<ClientUsage> {
        let mut rows: Vec<ClientUsage> = self
            .clients
            .iter()
            .map(|(client, t)| ClientUsage {
                client: client.clone(),
                requests: t.requests,
                ok: t.ok,
                errors: t.errors,
                tokens_in: t.tokens_in,
                tokens_out: t.tokens_out,
            })
            .collect();
        rows.sort_by(|a, b| {
            b.requests
                .cmp(&a.requests)
                .then((b.tokens_in + b.tokens_out).cmp(&(a.tokens_in + a.tokens_out)))
                .then(a.client.cmp(&b.client))
        });
        rows
    }

    /// Snapshot of every model row, sorted by total tokens desc, then requests,
    /// then key (req14). The document builder overlays in-flight counts.
    pub(crate) fn model_usage(&self) -> Vec<ModelUsage> {
        let mut rows: Vec<ModelUsage> = self
            .models
            .iter()
            .map(|((group, model), stats)| {
                let mut accounts: Vec<ModelAccount> = stats
                    .accounts
                    .iter()
                    .map(|(name, t)| ModelAccount {
                        name: name.clone(),
                        requests: t.requests,
                        ok: t.ok,
                        errors: t.errors,
                        tokens_in: t.tokens_in,
                        tokens_out: t.tokens_out,
                    })
                    .collect();
                accounts.sort_by(|a, b| b.requests.cmp(&a.requests).then(a.name.cmp(&b.name)));
                ModelUsage {
                    group: group.clone(),
                    model: model.clone(),
                    requests: stats.requests,
                    ok: stats.ok,
                    errors: stats.errors,
                    tokens_in: stats.tokens_in,
                    tokens_out: stats.tokens_out,
                    cache_read: stats.cache_read,
                    cache_creation: stats.cache_creation,
                    last_used: stats.last_used.unwrap_or(SystemTime::UNIX_EPOCH),
                    accounts,
                    efforts: sorted_counts(&stats.efforts),
                    endpoints: sorted_counts(&stats.endpoints),
                }
            })
            .collect();
        rows.sort_by(|a, b| {
            (b.tokens_in + b.tokens_out)
                .cmp(&(a.tokens_in + a.tokens_out))
                .then(b.requests.cmp(&a.requests))
                .then(a.group.cmp(&b.group))
                .then(a.model.cmp(&b.model))
        });
        rows
    }

    /// Aggregate the windowed bucket ring over `window` ending at `now`: one
    /// row per `(group, normalized_model, account)` with any activity in the
    /// window, sorted by total tokens desc (issue #23). Drives the heatmap.
    /// Best-effort — the underlying events are a lossy sample (dropped on a full
    /// activity channel), so these numbers may undercount.
    pub(crate) fn windowed_rows(&self, window: StatsWindow, now: SystemTime) -> Vec<WindowedRow> {
        self.windowed.aggregate(window, now)
    }

    /// Completed requests per minute over the trailing `window` (notes
    /// excluded). Bounded by the ring capacity: with the default 200-entry
    /// ring this is exact until ~200 requests land inside the window.
    pub(crate) fn requests_per_minute(&self, now: SystemTime, window: Duration) -> f64 {
        let minutes = window.as_secs_f64() / 60.0;
        if minutes <= 0.0 {
            return 0.0;
        }
        let cutoff = now.checked_sub(window);
        let count = self
            .completed
            .iter()
            .filter(|entry| matches!(entry.body, CompletedBody::Request { .. }))
            .filter(|entry| cutoff.is_none_or(|cutoff| entry.at >= cutoff))
            .count();
        count as f64 / minutes
    }

    /// Fold one proxy event into the log. `now` stamps the resulting entry.
    pub(crate) fn apply(&mut self, event: ActivityEvent, now: SystemTime) {
        // Backstop against a dropped `RequestFinished`: any row older than the
        // stale threshold is presumed finished before we fold the next event.
        self.prune_stale_in_flight(now);
        match event {
            ActivityEvent::RequestStarted { id, method, path } => {
                if self.in_flight.len() >= MAX_IN_FLIGHT {
                    let lost = self.in_flight.remove(0);
                    self.push_note(
                        format!(
                            "{} {} never finished (in-flight overflow)",
                            lost.method, lost.path
                        ),
                        true,
                        now,
                    );
                }
                self.in_flight.push(InFlight {
                    id,
                    method,
                    path,
                    account: None,
                    group: None,
                    model: None,
                    started_at: now,
                });
            }
            ActivityEvent::RequestRouted {
                id,
                account,
                group,
                model,
            } => {
                if let Some(entry) = self.in_flight.iter_mut().find(|r| r.id == id) {
                    entry.account = Some(account);
                    entry.group = group;
                    entry.model = model;
                }
            }
            ActivityEvent::RequestFinished {
                id,
                method,
                path,
                account,
                status,
                duration,
                tokens,
                group,
                model,
                effort,
                user_id,
            } => {
                let routed = self
                    .in_flight
                    .iter()
                    .position(|r| r.id == id)
                    .map(|i| self.in_flight.remove(i))
                    .and_then(|r| r.account);
                let account = account.or(routed);
                // Per-client attribution (issue #32): every finished request is
                // counted against its `metadata.user_id` client bucket (the
                // `unknown` bucket when absent), independent of routing — so
                // pre-routing failures are attributed too, never dropped.
                self.record_client(user_id.as_deref(), status, tokens);
                let bucket = match &account {
                    Some(name) => self.totals.entry(name.clone()).or_default(),
                    None => &mut self.unrouted,
                };
                bucket.requests += 1;
                if status < 400 {
                    bucket.ok += 1;
                } else {
                    bucket.errors += 1;
                }
                if let Some(tokens) = tokens {
                    bucket.tokens_in += tokens.input;
                    bucket.tokens_out += tokens.output;
                }
                // Model-usage aggregation (req1-20): only when the request was
                // attributed to a (group, model). Pre-routing failures keep
                // group/model None and stay in the global/unrouted accounting
                // above — no bogus model row. A failed-but-attributed request
                // still increments the row's error count even with no tokens.
                if let (Some(group), Some(model)) = (&group, &model) {
                    self.record_model(group, model, &account, status, tokens, &effort, &path, now);
                }
                self.push(Completed {
                    at: now,
                    body: CompletedBody::Request {
                        method,
                        path,
                        account,
                        status,
                        duration,
                        tokens,
                        group,
                        model,
                        effort,
                    },
                });
            }
            ActivityEvent::AccountSwitched { from, to, reason } => {
                let from = from.unwrap_or_else(|| "(none)".into());
                let why = reason.map(|r| format!(" ({r})")).unwrap_or_default();
                self.push_note(format!("switch {from} → {to}{why}"), false, now);
            }
            ActivityEvent::TokenRefreshed {
                account,
                expires_at_ms,
            } => {
                let expiry = std::time::UNIX_EPOCH + Duration::from_millis(expires_at_ms);
                let note = match expiry.duration_since(now) {
                    Ok(left) => format!(
                        "token refreshed: {account} (expires {})",
                        crate::scheduler::select::compact_duration(left)
                    ),
                    // Unknown (0) or already-past expiry: no suffix.
                    Err(_) => format!("token refreshed: {account}"),
                };
                self.push_note(note, false, now);
            }
            // Poller health is tracked by `App` (it feeds the poller pane,
            // not the activity list — one line per poll would drown it).
            ActivityEvent::UsagePolled { .. } => {}
            ActivityEvent::Error { context, message } => {
                let ctx = context.map(|c| format!("{c}: ")).unwrap_or_default();
                self.push_note(format!("{ctx}{message}"), true, now);
            }
        }
    }

    /// Append a TUI-internal note (reload result, switch attempt, …).
    pub(crate) fn push_note(&mut self, text: String, error: bool, now: SystemTime) {
        self.push(Completed {
            at: now,
            body: CompletedBody::Note { text, error },
        });
    }

    fn push(&mut self, entry: Completed) {
        self.completed.push_front(entry);
        self.completed.truncate(self.capacity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn note_text(entry: &Completed) -> &str {
        match &entry.body {
            CompletedBody::Note { text, .. } => text,
            other => panic!("expected note, got {other:?}"),
        }
    }

    fn started(id: u64) -> ActivityEvent {
        ActivityEvent::RequestStarted {
            id,
            method: "POST".into(),
            path: "/v1/messages".into(),
        }
    }

    fn finished(id: u64, account: Option<&str>, tokens: Option<(u64, u64)>) -> ActivityEvent {
        finished_status(id, account, tokens, 200)
    }

    fn finished_status(
        id: u64,
        account: Option<&str>,
        tokens: Option<(u64, u64)>,
        status: u16,
    ) -> ActivityEvent {
        ActivityEvent::RequestFinished {
            id,
            method: "POST".into(),
            path: "/v1/messages".into(),
            account: account.map(str::to_string),
            status,
            duration: Duration::from_millis(1_400),
            tokens: tokens.map(|(input, output)| TokenCounts {
                input,
                output,
                ..Default::default()
            }),
            group: None,
            model: None,
            effort: None,
            user_id: None,
        }
    }

    /// A finished request attributed to a `(group, model)`, with optional
    /// effort and cache counters, for the model-aggregation tests.
    #[allow(clippy::too_many_arguments)]
    fn finished_model(
        id: u64,
        account: Option<&str>,
        group: &str,
        model: &str,
        effort: Option<&str>,
        status: u16,
        tokens: Option<TokenCounts>,
        path: &str,
    ) -> ActivityEvent {
        ActivityEvent::RequestFinished {
            id,
            method: "POST".into(),
            path: path.into(),
            account: account.map(str::to_string),
            status,
            duration: Duration::from_millis(1_400),
            tokens,
            group: Some(group.into()),
            model: Some(model.into()),
            effort: effort.map(str::to_string),
            user_id: None,
        }
    }

    /// A finished request carrying a `metadata.user_id` (or `None`) for the
    /// per-client attribution tests (issue #32). Minimal otherwise.
    fn finished_client(
        id: u64,
        user_id: Option<&str>,
        tokens: Option<(u64, u64)>,
        status: u16,
    ) -> ActivityEvent {
        ActivityEvent::RequestFinished {
            id,
            method: "POST".into(),
            path: "/v1/messages".into(),
            account: None,
            status,
            duration: Duration::from_millis(1_400),
            tokens: tokens.map(|(input, output)| TokenCounts {
                input,
                output,
                ..Default::default()
            }),
            group: None,
            model: None,
            effort: None,
            user_id: user_id.map(str::to_string),
        }
    }

    // ---- ring buffer behavior ----

    #[test]
    fn ring_buffer_evicts_oldest_and_orders_newest_first() {
        let mut log = ActivityLog::new(3);
        for i in 0..4 {
            log.push_note(format!("note-{i}"), false, at(i));
        }
        let texts: Vec<&str> = log.completed().map(note_text).collect();
        assert_eq!(
            texts,
            vec!["note-3", "note-2", "note-1"],
            "newest first, oldest evicted"
        );
    }

    #[test]
    fn capacity_is_respected_under_mixed_events() {
        let mut log = ActivityLog::new(2);
        log.apply(started(1), at(0));
        log.apply(finished(1, Some("a"), None), at(1));
        log.push_note("one".into(), false, at(2));
        log.push_note("two".into(), false, at(3));
        assert_eq!(log.completed().count(), 2);
        assert_eq!(note_text(log.completed().next().expect("entry")), "two");
    }

    // ---- request lifecycle ----

    #[test]
    fn started_request_is_in_flight_until_finished() {
        let mut log = ActivityLog::new(10);
        log.apply(started(7), at(0));
        assert_eq!(log.in_flight().len(), 1);
        assert_eq!(log.in_flight()[0].account, None);

        log.apply(
            ActivityEvent::RequestRouted {
                id: 7,
                account: "a@x.com".into(),
                group: Some("claude".into()),
                model: Some("claude-sonnet-4-5".into()),
            },
            at(1),
        );
        assert_eq!(log.in_flight()[0].account.as_deref(), Some("a@x.com"));
        assert_eq!(log.in_flight()[0].group.as_deref(), Some("claude"));
        assert_eq!(
            log.in_flight()[0].model.as_deref(),
            Some("claude-sonnet-4-5")
        );

        // Finish without an explicit account: the routed account is kept.
        log.apply(finished(7, None, Some((1_000, 200))), at(2));
        assert!(log.in_flight().is_empty(), "finish clears the spinner row");
        let entry = log.completed().next().expect("completed entry").clone();
        match &entry.body {
            CompletedBody::Request {
                account,
                status,
                tokens,
                ..
            } => {
                assert_eq!(account.as_deref(), Some("a@x.com"));
                assert_eq!(*status, 200);
                assert_eq!(
                    *tokens,
                    Some(TokenCounts {
                        input: 1_000,
                        output: 200,
                        ..Default::default()
                    })
                );
            }
            other => panic!("expected request entry, got {other:?}"),
        }
    }

    #[test]
    fn finish_without_matching_start_still_logs() {
        let mut log = ActivityLog::new(10);
        log.apply(finished(99, Some("b"), None), at(0));
        assert_eq!(log.completed().count(), 1);
        assert!(log.in_flight().is_empty());
    }

    #[test]
    fn in_flight_overflow_retires_oldest_as_error_note() {
        let mut log = ActivityLog::new(200);
        for id in 0..(MAX_IN_FLIGHT as u64 + 1) {
            log.apply(started(id), at(id));
        }
        assert_eq!(log.in_flight().len(), MAX_IN_FLIGHT);
        assert!(!log.in_flight().iter().any(|r| r.id == 0), "oldest dropped");
        let entry = log.completed().next().expect("note").clone();
        match &entry.body {
            CompletedBody::Note { error, .. } => assert!(error),
            other => panic!("expected note, got {other:?}"),
        }
    }

    #[test]
    fn prune_stale_in_flight_sweeps_rows_past_threshold_with_a_note() {
        let mut log = ActivityLog::new(200);
        log.apply(started(1), at(0));
        assert_eq!(log.in_flight().len(), 1, "row is in-flight");

        // Still fresh just before the threshold: nothing swept.
        log.prune_stale_in_flight(at(STALE_IN_FLIGHT.as_secs() - 1));
        assert_eq!(log.in_flight().len(), 1, "not yet stale");

        // Advance past the stale threshold (real requests finish in <90s, so a
        // row this old means its RequestFinished was dropped).
        log.prune_stale_in_flight(at(STALE_IN_FLIGHT.as_secs() + 1));
        assert!(
            log.in_flight().is_empty(),
            "stale row swept, no zombie left"
        );
        let entry = log.completed().next().expect("sweep note").clone();
        match &entry.body {
            CompletedBody::Note { text, error } => {
                assert!(error, "sweep note is an error note");
                assert!(
                    text.contains("presumed finished"),
                    "note names the cause, got {text:?}"
                );
            }
            other => panic!("expected note, got {other:?}"),
        }
    }

    #[test]
    fn apply_sweeps_stale_in_flight_before_folding_next_event() {
        let mut log = ActivityLog::new(200);
        log.apply(started(1), at(0));
        // A later, unrelated event arriving past the threshold sweeps the
        // leaked row even though no RequestFinished for id 1 ever came.
        log.apply(started(2), at(STALE_IN_FLIGHT.as_secs() + 5));
        assert!(
            !log.in_flight().iter().any(|r| r.id == 1),
            "leaked row 1 swept on the next apply"
        );
        assert!(
            log.in_flight().iter().any(|r| r.id == 2),
            "fresh row 2 still in-flight"
        );
    }

    // ---- totals ----

    #[test]
    fn totals_accumulate_per_account_with_ok_error_and_token_split() {
        let mut log = ActivityLog::new(10);
        log.apply(started(1), at(0));
        log.apply(finished(1, Some("a"), Some((700, 300))), at(1));
        log.apply(started(2), at(2));
        log.apply(finished(2, Some("a"), None), at(3)); // unknown tokens count 0
        log.apply(finished_status(3, Some("a"), None, 502), at(4));
        log.apply(finished(4, Some("b"), Some((20, 30))), at(5));

        assert_eq!(
            log.totals_for("a"),
            Totals {
                requests: 3,
                ok: 2,
                errors: 1,
                tokens_in: 700,
                tokens_out: 300,
            }
        );
        assert_eq!(log.totals_for("a").tokens(), 1_000);
        assert_eq!(
            log.totals_for("b"),
            Totals {
                requests: 1,
                ok: 1,
                errors: 0,
                tokens_in: 20,
                tokens_out: 30,
            }
        );
        assert_eq!(log.totals_for("ghost"), Totals::default());
    }

    #[test]
    fn unrouted_failure_counts_globally_but_not_per_account() {
        let mut log = ActivityLog::new(10);
        log.apply(started(1), at(0));
        log.apply(finished_status(1, None, None, 429), at(1)); // never routed
        log.apply(finished(2, Some("a"), Some((5, 5))), at(2));
        assert_eq!(log.totals_for("a").requests, 1);
        assert_eq!(
            log.totals_global(),
            Totals {
                requests: 2,
                ok: 1,
                errors: 1,
                tokens_in: 5,
                tokens_out: 5,
            }
        );
    }

    // ---- requests per minute ----

    #[test]
    fn rpm_counts_only_requests_inside_the_window() {
        let mut log = ActivityLog::new(50);
        let now = at(1_000);
        // 3 requests inside the 5m window, 1 outside, plus a note (ignored).
        log.apply(finished(1, Some("a"), None), at(1_000 - 400)); // outside
        log.apply(finished(2, Some("a"), None), at(1_000 - 200));
        log.apply(finished(3, Some("a"), None), at(1_000 - 100));
        log.apply(finished(4, Some("a"), None), at(1_000));
        log.push_note("switch".into(), false, at(1_000 - 50));

        let rpm = log.requests_per_minute(now, Duration::from_secs(300));
        assert!((rpm - 3.0 / 5.0).abs() < 1e-9, "got {rpm}");
    }

    #[test]
    fn rpm_zero_window_and_empty_log_are_zero() {
        let log = ActivityLog::new(10);
        assert_eq!(
            log.requests_per_minute(at(1_000), Duration::from_secs(300)),
            0.0
        );
        let mut log = ActivityLog::new(10);
        log.apply(finished(1, Some("a"), None), at(1_000));
        assert_eq!(log.requests_per_minute(at(1_000), Duration::ZERO), 0.0);
    }

    #[test]
    fn usage_polled_is_not_an_activity_line() {
        let mut log = ActivityLog::new(10);
        log.apply(
            ActivityEvent::UsagePolled {
                account: "a".into(),
                ok: true,
                consecutive_failures: 0,
                next_in: Duration::from_secs(300),
            },
            at(0),
        );
        assert_eq!(log.completed().count(), 0);
    }

    // ---- model usage aggregation ----

    fn tokens(input: u64, output: u64, cache_read: Option<u64>) -> Option<TokenCounts> {
        Some(TokenCounts {
            input,
            output,
            cache_read,
            cache_creation: None,
        })
    }

    #[test]
    fn endpoint_class_buckets_count_tokens_messages_and_other() {
        assert_eq!(endpoint_class("/v1/messages"), "messages");
        assert_eq!(endpoint_class("/v1/messages?beta=true"), "messages");
        assert_eq!(endpoint_class("/v1/messages/count_tokens"), "count_tokens");
        assert_eq!(endpoint_class("/v1/models"), "models");
    }

    #[test]
    fn normalize_model_strips_context_suffix() {
        assert_eq!(
            normalize_model("claude-sonnet-4-5[1m]"),
            "claude-sonnet-4-5"
        );
        assert_eq!(normalize_model("gpt-5.5"), "gpt-5.5");
    }

    #[test]
    fn model_rows_key_by_group_and_served_model() {
        let mut log = ActivityLog::new(50);
        // Same label, different providers → two rows, never merged (req1/2).
        log.apply(
            finished_model(
                1,
                Some("a"),
                "claude",
                "shared",
                None,
                200,
                tokens(10, 5, None),
                "/v1/messages",
            ),
            at(1),
        );
        log.apply(
            finished_model(
                2,
                Some("c"),
                "codex",
                "shared",
                None,
                200,
                tokens(20, 7, None),
                "/v1/messages",
            ),
            at(2),
        );
        let rows = log.model_usage();
        assert_eq!(rows.len(), 2);
        // Sorted by total tokens desc → codex (27) before claude (15).
        assert_eq!(
            (rows[0].group.as_str(), rows[0].model.as_str()),
            ("codex", "shared")
        );
        assert_eq!(
            (rows[1].group.as_str(), rows[1].model.as_str()),
            ("claude", "shared")
        );
    }

    #[test]
    fn model_row_accumulates_split_cache_effort_endpoint_and_accounts() {
        let mut log = ActivityLog::new(50);
        log.apply(
            finished_model(
                1,
                Some("a"),
                "claude",
                "claude-sonnet-4-5[1m]",
                Some("16k"),
                200,
                tokens(100, 40, Some(900)),
                "/v1/messages",
            ),
            at(10),
        );
        log.apply(
            finished_model(
                2,
                Some("b"),
                "claude",
                "claude-sonnet-4-5",
                None,
                200,
                tokens(50, 20, None),
                "/v1/messages/count_tokens",
            ),
            at(20),
        );
        // A failed request with a known model: error count, no tokens (req-test).
        log.apply(
            finished_model(
                3,
                Some("a"),
                "claude",
                "claude-sonnet-4-5",
                Some("16k"),
                529,
                None,
                "/v1/messages",
            ),
            at(30),
        );
        let rows = log.model_usage();
        // Suffix normalization merges into one row (req17).
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.requests, 3);
        assert_eq!(row.ok, 2);
        assert_eq!(row.errors, 1);
        assert_eq!(row.tokens_in, 150);
        assert_eq!(row.tokens_out, 60);
        // cache_read present from req1 only; cache_creation never reported.
        assert_eq!(row.cache_read, Some(900));
        assert_eq!(row.cache_creation, None);
        assert_eq!(row.last_used, at(30));
        // Effort distribution: 16k×2, none×1.
        let effort: HashMap<&str, u64> = row
            .efforts
            .iter()
            .map(|c| (c.label.as_str(), c.requests))
            .collect();
        assert_eq!(effort.get("16k"), Some(&2));
        assert_eq!(effort.get("none"), Some(&1));
        // Endpoint split: messages×2, count_tokens×1.
        let endpoint: HashMap<&str, u64> = row
            .endpoints
            .iter()
            .map(|c| (c.label.as_str(), c.requests))
            .collect();
        assert_eq!(endpoint.get("messages"), Some(&2));
        assert_eq!(endpoint.get("count_tokens"), Some(&1));
        // Per-account: a served 2 (one failed), b served 1.
        let a = row
            .accounts
            .iter()
            .find(|x| x.name == "a")
            .expect("account a");
        assert_eq!((a.requests, a.ok, a.errors, a.tokens_in), (2, 1, 1, 100));
        let b = row
            .accounts
            .iter()
            .find(|x| x.name == "b")
            .expect("account b");
        assert_eq!((b.requests, b.tokens_in), (1, 50));
    }

    #[test]
    fn pre_routing_failure_does_not_create_a_model_row() {
        let mut log = ActivityLog::new(50);
        // No group/model (body-read failure): global accounting only, no row.
        log.apply(finished_status(1, None, None, 400), at(1));
        assert!(log.model_usage().is_empty());
        assert_eq!(log.totals_global().requests, 1);
    }

    // ---- per-client attribution (issue #32) ----

    #[test]
    fn client_attribution_counts_requests_and_tokens_per_user_id() {
        let mut log = ActivityLog::new(50);
        // alice: 2 requests (one a 502 error), 300 in / 110 out total.
        log.apply(
            finished_client(1, Some("alice"), Some((100, 40)), 200),
            at(1),
        );
        log.apply(
            finished_client(2, Some("alice"), Some((200, 70)), 502),
            at(2),
        );
        // bob: 1 ok request.
        log.apply(finished_client(3, Some("bob"), Some((10, 5)), 200), at(3));
        // Two requests with NO user_id land in the explicit `unknown` bucket
        // (one carries no tokens), never dropped.
        log.apply(finished_client(4, None, Some((7, 3)), 200), at(4));
        log.apply(finished_client(5, None, None, 200), at(5));

        assert_eq!(
            log.client_totals("alice"),
            Totals {
                requests: 2,
                ok: 1,
                errors: 1,
                tokens_in: 300,
                tokens_out: 110,
            }
        );
        assert_eq!(
            log.client_totals("bob"),
            Totals {
                requests: 1,
                ok: 1,
                errors: 0,
                tokens_in: 10,
                tokens_out: 5,
            }
        );
        // No-user_id requests attributed to `unknown`, not dropped.
        assert_eq!(
            log.client_totals(UNKNOWN_CLIENT),
            Totals {
                requests: 2,
                ok: 2,
                errors: 0,
                tokens_in: 7,
                tokens_out: 3,
            }
        );
        // An empty-string user_id is treated as no id → `unknown`.
        log.apply(finished_client(6, Some(""), Some((1, 1)), 200), at(6));
        assert_eq!(log.client_totals(UNKNOWN_CLIENT).requests, 3);

        // client_usage() snapshot: three rows, sorted by requests desc — the
        // total across rows equals the number of finished requests (none lost).
        let rows = log.client_usage();
        assert_eq!(rows.len(), 3, "alice, bob, unknown");
        let total_requests: u64 = rows.iter().map(|r| r.requests).sum();
        assert_eq!(total_requests, 6, "every finished request attributed");
        // alice (2) and unknown (3) outrank bob (1); unknown leads on requests.
        assert_eq!(rows[0].client, UNKNOWN_CLIENT);
        assert_eq!(rows[0].requests, 3);
    }

    #[test]
    fn client_attribution_is_bounded_overflow_folds_into_unknown() {
        let mut log = ActivityLog::new(50);
        // Fill the named-client cap exactly with distinct ids.
        for i in 0..MAX_CLIENTS {
            log.apply(
                finished_client(i as u64, Some(&format!("c{i}")), None, 200),
                at(i as u64),
            );
        }
        assert_eq!(
            log.client_usage().len(),
            MAX_CLIENTS,
            "every distinct id under the cap gets its own bucket"
        );
        // A brand-new id past the cap does NOT allocate a new entry; it is
        // folded into `unknown` so the map cannot grow unbounded.
        log.apply(
            finished_client(9_000, Some("overflow"), None, 200),
            at(9_000),
        );
        assert_eq!(
            log.client_totals("overflow"),
            Totals::default(),
            "over-cap id is not tracked on its own"
        );
        assert_eq!(
            log.client_totals(UNKNOWN_CLIENT).requests,
            1,
            "over-cap id folded into unknown"
        );
        // An ALREADY-tracked id keeps accumulating even past the cap.
        log.apply(finished_client(9_001, Some("c0"), None, 200), at(9_001));
        assert_eq!(log.client_totals("c0").requests, 2);
    }

    // ---- windowed bucket ring (issue #23) ----

    /// One bucketed (group, model, account) cell within a window's aggregate.
    fn cell<'a>(
        rows: &'a [WindowedRow],
        group: &str,
        model: &str,
        account: &str,
    ) -> Option<&'a WindowCounts> {
        rows.iter()
            .find(|r| r.group == group && r.model == model && r.account == account)
            .map(|r| &r.counts)
    }

    /// `now` `hours` after the epoch, for window arithmetic at hour resolution.
    fn at_hours(hours: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(hours * 3600)
    }

    #[test]
    fn windowed_aggregates_per_group_model_account_are_correct() {
        let mut log = ActivityLog::new(LOG_CAPACITY);
        // Three requests for claude/sonnet on account "a" inside the last hour.
        for id in 0..3u64 {
            log.apply(
                finished_model(
                    id,
                    Some("a"),
                    "claude",
                    "claude-sonnet-4-5[1m]", // suffix is normalized away
                    None,
                    200,
                    tokens(100, 40, Some(10)),
                    "/v1/messages",
                ),
                at_hours(100),
            );
        }
        // One failed request for the SAME model but account "b".
        log.apply(
            finished_model(
                10,
                Some("b"),
                "claude",
                "claude-sonnet-4-5",
                None,
                500,
                None,
                "/v1/messages",
            ),
            at_hours(100),
        );
        let now = at_hours(100);
        let rows = log.windowed_rows(StatsWindow::Day, now);

        let a = cell(&rows, "claude", "claude-sonnet-4-5", "a").expect("a cell");
        assert_eq!(a.requests, 3);
        assert_eq!(a.ok, 3);
        assert_eq!(a.errors, 0);
        assert_eq!(a.tokens_in, 300);
        assert_eq!(a.tokens_out, 120);
        assert_eq!(a.cache_read, 30);
        // tokens() = in + out + cache_read + cache_creation.
        assert_eq!(a.tokens(), 450);

        let b = cell(&rows, "claude", "claude-sonnet-4-5", "b").expect("b cell");
        assert_eq!((b.requests, b.ok, b.errors), (1, 0, 1));
        assert_eq!(b.tokens(), 0, "failed request carried no tokens");

        // Two distinct account cells for one (group, model).
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn windowed_same_model_under_two_groups_stays_two_rows() {
        let mut log = ActivityLog::new(LOG_CAPACITY);
        log.apply(
            finished_model(
                1,
                Some("a"),
                "claude",
                "shared",
                None,
                200,
                tokens(10, 5, None),
                "/v1/messages",
            ),
            at_hours(50),
        );
        log.apply(
            finished_model(
                2,
                Some("a"),
                "codex",
                "shared",
                None,
                200,
                tokens(20, 7, None),
                "/v1/messages",
            ),
            at_hours(50),
        );
        let rows = log.windowed_rows(StatsWindow::Day, at_hours(50));
        // Same model name, same account, different group → never merged.
        assert!(cell(&rows, "claude", "shared", "a").is_some());
        assert!(cell(&rows, "codex", "shared", "a").is_some());
        assert_eq!(rows.len(), 2, "dropping group would have merged to 1");
    }

    #[test]
    fn windowed_24h_and_72h_select_the_right_buckets() {
        let mut log = ActivityLog::new(LOG_CAPACITY);
        // t = 200h: an old request, ~50h before "now".
        log.apply(
            finished_model(
                1,
                Some("a"),
                "claude",
                "m",
                None,
                200,
                tokens(7, 0, None),
                "/v1/messages",
            ),
            at_hours(200),
        );
        // t = 240h: a request 10h before "now".
        log.apply(
            finished_model(
                2,
                Some("a"),
                "claude",
                "m",
                None,
                200,
                tokens(11, 0, None),
                "/v1/messages",
            ),
            at_hours(240),
        );
        let now = at_hours(250);

        // 24h window: only the t=240h request (10h ago) is inside.
        let day = log.windowed_rows(StatsWindow::Day, now);
        assert_eq!(cell(&day, "claude", "m", "a").expect("day").requests, 1);
        assert_eq!(cell(&day, "claude", "m", "a").expect("day").tokens_in, 11);

        // 72h window: both the 10h-ago and 50h-ago requests are inside.
        let three = log.windowed_rows(StatsWindow::ThreeDay, now);
        assert_eq!(cell(&three, "claude", "m", "a").expect("3d").requests, 2);
        assert_eq!(cell(&three, "claude", "m", "a").expect("3d").tokens_in, 18);
    }

    #[test]
    fn windowed_roll_forward_expires_old_buckets_and_prunes_empty_keys() {
        let mut log = ActivityLog::new(LOG_CAPACITY);
        // A stray/typo model key recorded long ago.
        log.apply(
            finished_model(
                1,
                Some("a"),
                "claude",
                "typo-model",
                None,
                200,
                tokens(5, 0, None),
                "/v1/messages",
            ),
            at_hours(10),
        );
        // Far in the future (well past the 73-bucket retention): record again so
        // roll-forward advances the ring past the stray key's bucket.
        log.apply(
            finished_model(
                2,
                Some("a"),
                "claude",
                "live-model",
                None,
                200,
                tokens(9, 0, None),
                "/v1/messages",
            ),
            at_hours(10 + BUCKET_COUNT as u64 + 5),
        );
        let now = at_hours(10 + BUCKET_COUNT as u64 + 5);

        // The stray key's bucket was pruned entirely (not zeroed) — it is gone
        // from every window, so a typo key cannot grow the ring unbounded.
        let three = log.windowed_rows(StatsWindow::ThreeDay, now);
        assert!(
            cell(&three, "claude", "typo-model", "a").is_none(),
            "expired bucket must be pruned, not retained as a zero key"
        );
        assert!(
            cell(&three, "claude", "live-model", "a").is_some(),
            "the recent key survives"
        );
        // Internally: no empty key map lingers (pruned wholesale).
        assert!(
            log.windowed.buckets.iter().all(|b| !b.counts.is_empty()),
            "no empty bucket retained after roll-forward"
        );
    }

    #[test]
    fn windowed_is_defensive_against_backwards_clock_skew() {
        let mut log = ActivityLog::new(LOG_CAPACITY);
        log.apply(
            finished_model(
                1,
                Some("a"),
                "claude",
                "m",
                None,
                200,
                tokens(10, 0, None),
                "/v1/messages",
            ),
            at_hours(100),
        );
        // A later event with an EARLIER timestamp (NTP step back) must not panic
        // and must still be counted somewhere in the ring.
        log.apply(
            finished_model(
                2,
                Some("a"),
                "claude",
                "m",
                None,
                200,
                tokens(3, 0, None),
                "/v1/messages",
            ),
            at_hours(99),
        );
        // A pre-epoch timestamp clamps to hour 0 rather than panicking.
        let rows = log.windowed_rows(StatsWindow::ThreeDay, at_hours(100));
        assert!(
            cell(&rows, "claude", "m", "a").is_some(),
            "skewed events still fold without panic"
        );
    }

    // ---- persistence (req-persist A/C) ----

    use std::path::{Path, PathBuf};

    /// Self-cleaning unique temp dir (no tempfile dev-dependency), mirroring
    /// the pattern in `config::tests` / `server::tests`.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "llmux-activity-test-{}-{}",
                std::process::id(),
                ulid::Ulid::new()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            Self(dir)
        }
        fn file(&self) -> PathBuf {
            self.0.join("activity.jsonl")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A finished, fully-attributed request for the persistence round-trip:
    /// exercises account totals AND a `(group, model)` model row + cache split.
    #[allow(clippy::too_many_arguments)]
    fn finished_full(
        id: u64,
        account: &str,
        group: &str,
        model: &str,
        effort: Option<&str>,
        status: u16,
        input: u64,
        output: u64,
        cache_read: Option<u64>,
        path: &str,
    ) -> ActivityEvent {
        ActivityEvent::RequestFinished {
            id,
            method: "POST".into(),
            path: path.into(),
            account: Some(account.to_string()),
            status,
            duration: Duration::from_millis(1_234),
            tokens: Some(TokenCounts {
                input,
                output,
                cache_read,
                cache_creation: None,
            }),
            group: Some(group.into()),
            model: Some(model.into()),
            effort: effort.map(str::to_string),
            // A per-client id so the persistence round-trip also exercises the
            // issue #32 client attribution (one client id per account here).
            user_id: Some(format!("client-{account}")),
        }
    }

    /// Apply the same events to a fresh log without persisting — the oracle the
    /// restored log must match exactly.
    fn live_log(events: &[(ActivityEvent, SystemTime)]) -> ActivityLog {
        let mut log = ActivityLog::new(LOG_CAPACITY);
        for (event, ts) in events {
            log.apply(event.clone(), *ts);
        }
        log
    }

    /// Persist each event to `path`, then load a FRESH log from it.
    fn persisted_then_loaded(path: &Path, events: &[(ActivityEvent, SystemTime)]) -> ActivityLog {
        for (event, ts) in events {
            persist_request(Some(path), event, *ts);
        }
        let mut log = ActivityLog::new(LOG_CAPACITY);
        log.load_persisted(Some(path));
        log
    }

    /// Compare two logs on every persisted-aggregate surface: model_usage,
    /// global totals, and per-account totals. (model_usage carries last_used,
    /// cache split, accounts, efforts, endpoints — so equality here is strong.)
    fn assert_same_aggregates(a: &ActivityLog, b: &ActivityLog, accounts: &[&str]) {
        assert_eq!(
            a.model_usage(),
            b.model_usage(),
            "model_usage must match after restore"
        );
        assert_eq!(
            a.totals_global(),
            b.totals_global(),
            "global totals must match after restore"
        );
        assert_eq!(
            a.client_usage(),
            b.client_usage(),
            "per-client attribution must match after restore (issue #32)"
        );
        for acct in accounts {
            assert_eq!(
                a.totals_for(acct),
                b.totals_for(acct),
                "per-account totals for {acct} must match after restore"
            );
        }
    }

    #[test]
    fn persisted_round_trip_rebuilds_identical_aggregates() {
        let tmp = TempDir::new();
        let path = tmp.file();
        let events = vec![
            (
                finished_full(
                    1,
                    "a",
                    "claude",
                    "claude-sonnet-4-5[1m]",
                    Some("16k"),
                    200,
                    700,
                    300,
                    Some(900),
                    "/v1/messages",
                ),
                at(10),
            ),
            (
                finished_full(
                    2,
                    "b",
                    "codex",
                    "gpt-5.5",
                    Some("high"),
                    200,
                    50,
                    20,
                    None,
                    "/v1/messages",
                ),
                at(20),
            ),
            (
                finished_full(
                    3,
                    "a",
                    "claude",
                    "claude-sonnet-4-5",
                    None,
                    529,
                    0,
                    0,
                    None,
                    "/v1/messages/count_tokens",
                ),
                at(30),
            ),
        ];

        let live = live_log(&events);
        let restored = persisted_then_loaded(&path, &events);

        assert_same_aggregates(&live, &restored, &["a", "b", "ghost"]);
        // Sanity: the restore is non-trivial (two model rows, three requests).
        assert_eq!(restored.model_usage().len(), 2);
        assert_eq!(restored.totals_global().requests, 3);
    }

    #[test]
    fn stats_continue_cumulatively_across_a_restart() {
        let tmp = TempDir::new();
        let path = tmp.file();

        // Session 1: N events, persisted as they fold.
        let session1 = vec![
            (
                finished_full(
                    1,
                    "a",
                    "claude",
                    "sonnet",
                    Some("16k"),
                    200,
                    100,
                    40,
                    Some(10),
                    "/v1/messages",
                ),
                at(10),
            ),
            (
                finished_full(
                    2,
                    "a",
                    "claude",
                    "sonnet",
                    None,
                    200,
                    200,
                    60,
                    None,
                    "/v1/messages",
                ),
                at(20),
            ),
        ];
        {
            let mut log1 = ActivityLog::new(LOG_CAPACITY);
            for (event, ts) in &session1 {
                persist_request(Some(&path), event, *ts);
                log1.apply(event.clone(), *ts);
            }
            // log1 dropped here — simulates daemon restart.
        }

        // Session 2: load the persisted log (resume), then M more events.
        let mut log2 = ActivityLog::new(LOG_CAPACITY);
        log2.load_persisted(Some(&path));
        let session2 = vec![
            (
                finished_full(
                    3,
                    "a",
                    "claude",
                    "sonnet",
                    None,
                    200,
                    300,
                    90,
                    None,
                    "/v1/messages",
                ),
                at(30),
            ),
            (
                finished_full(
                    4,
                    "b",
                    "codex",
                    "gpt-5.5",
                    None,
                    200,
                    5,
                    5,
                    None,
                    "/v1/messages",
                ),
                at(40),
            ),
        ];
        for (event, ts) in &session2 {
            log2.apply(event.clone(), *ts);
        }

        // Totals must equal ALL N+M events, not reset to just session 2.
        let mut all = session1.clone();
        all.extend(session2.clone());
        let oracle = live_log(&all);
        assert_same_aggregates(&oracle, &log2, &["a", "b"]);
        assert_eq!(
            log2.totals_global().requests,
            4,
            "stats continue, not reset"
        );
        // Account a: 3 requests, 600 in / 190 out across both sessions.
        assert_eq!(log2.totals_for("a").requests, 3);
        assert_eq!(log2.totals_for("a").tokens_in, 600);
        assert_eq!(log2.totals_for("a").tokens_out, 190);
    }

    #[test]
    fn corrupt_and_old_lines_are_tolerated() {
        let tmp = TempDir::new();
        let path = tmp.file();

        // A valid line (write it through the real persist path).
        persist_request(
            Some(&path),
            &finished_full(
                1,
                "a",
                "claude",
                "sonnet",
                None,
                200,
                10,
                5,
                None,
                "/v1/messages",
            ),
            at(10),
        );
        // Append garbage + a blank line + a wrong-version line by hand.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("reopen");
            writeln!(f, "this is not json {{").expect("write garbage");
            writeln!(f).expect("write blank");
            // Structurally valid JSON but a future/unknown schema version.
            writeln!(
                f,
                r#"{{"v":99,"ts_ms":1,"id":7,"method":"POST","path":"/x","account":null,"status":200,"duration_ms":1,"tokens":null,"group":null,"model":null,"effort":null}}"#
            )
            .expect("write old-version");
        }
        // Another valid line after the junk.
        persist_request(
            Some(&path),
            &finished_full(
                2,
                "b",
                "codex",
                "gpt-5.5",
                None,
                200,
                20,
                8,
                None,
                "/v1/messages",
            ),
            at(20),
        );

        let mut log = ActivityLog::new(LOG_CAPACITY);
        log.load_persisted(Some(&path)); // must not panic
                                         // Only the two valid lines loaded; garbage + v99 skipped.
        assert_eq!(log.totals_global().requests, 2);
        assert_eq!(log.totals_for("a").requests, 1);
        assert_eq!(log.totals_for("b").requests, 1);
        assert!(
            !log.model_usage().iter().any(|m| m.requests == 0),
            "no phantom row from the skipped v99 line"
        );
    }

    #[test]
    fn persistence_is_best_effort_none_and_unwritable_paths_never_panic() {
        // None path: persist + load are silent no-ops, fold still works.
        let mut log = ActivityLog::new(LOG_CAPACITY);
        let event = finished_full(
            1,
            "a",
            "claude",
            "sonnet",
            None,
            200,
            10,
            5,
            None,
            "/v1/messages",
        );
        persist_request(None, &event, at(10)); // no panic, nothing written
        log.load_persisted(None); // no panic, no-op
        log.apply(event, at(10)); // in-memory fold unaffected
        assert_eq!(log.totals_global().requests, 1);

        // Unwritable path: the parent is a *file*, so create_dir_all + open
        // both fail — swallowed, no panic, in-memory state untouched.
        let tmp = TempDir::new();
        let blocker = tmp.0.join("not-a-dir");
        std::fs::write(&blocker, b"x").expect("seed blocker file");
        let bad = blocker.join("activity.jsonl"); // parent is a file
        let mut log2 = ActivityLog::new(LOG_CAPACITY);
        persist_request(
            Some(&bad),
            &finished_full(
                2,
                "a",
                "claude",
                "sonnet",
                None,
                200,
                1,
                1,
                None,
                "/v1/messages",
            ),
            at(20),
        );
        log2.load_persisted(Some(&bad)); // read fails → no-op, no panic
        assert_eq!(
            log2.totals_global().requests,
            0,
            "unwritable path wrote nothing"
        );
    }
}
