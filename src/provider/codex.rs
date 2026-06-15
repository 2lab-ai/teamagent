//! OpenAI Codex provider (minimum viable, Phase C): serves Anthropic
//! Messages API requests from a ChatGPT-subscription Codex account by
//! translating to the Responses API (`POST {base}/responses`, model pinned
//! to [`CODEX_MODEL`]) and converting the Responses SSE stream back into
//! Anthropic SSE on the fly.
//!
//! Translation logic ported from CLIProxyAPI's `codex_claude_request.go` /
//! `codex_claude_response.go` shapes. The Anthropic passthrough never touches
//! this module — its byte-identity path is unchanged.

use bytes::Bytes;
use http::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use http::{HeaderMap, HeaderValue, Method};
use serde_json::{json, Map, Value};

use super::{ProviderError, ProviderRequest};
use crate::config::AccountCredential;
use crate::proxy::sse::{SseTransform, StreamUsage};

/// Fallback model slug when none is configured. The configurable default now
/// lives in `config.codex.default_model`; this const is the compile-time
/// fallback and the value [`CodexShape::default`] uses (so tests and the
/// `new(base_url)` constructor preserve the original pinned behavior).
pub const CODEX_MODEL: &str = "gpt-5.5";

/// Request path appended to the configured codex upstream.
pub const RESPONSES_PATH: &str = "/responses";

/// Request-shaping knobs for the Responses request, sourced from
/// `config.codex`. They mirror exactly what the codex CLI sets on the wire:
/// the model slug, `service_tier: "priority"` when `fast`, and a
/// `reasoning.effort` value. [`Default`] reproduces the original behavior
/// (pinned `gpt-5.5`, no fast tier, backend-default effort) so existing tests
/// and the bare `new` constructor are unaffected.
#[derive(Debug, Clone)]
pub struct CodexShape {
    /// Model slug requested upstream.
    pub model: String,
    /// `true` → send `service_tier: "priority"` (codex "fast" mode).
    pub fast: bool,
    /// `reasoning.effort` value (`none|minimal|low|medium|high|xhigh`), or
    /// `None` to omit the field and let the backend choose.
    pub effort: Option<String>,
}

impl Default for CodexShape {
    fn default() -> Self {
        Self {
            model: CODEX_MODEL.to_string(),
            fast: false,
            effort: None,
        }
    }
}

impl CodexShape {
    /// Build from the on-disk codex config.
    pub fn from_config(codex: &crate::config::schema::CodexConfig) -> Self {
        Self {
            model: codex.default_model.clone(),
            fast: codex.fast,
            effort: codex.reasoning_effort.clone(),
        }
    }
}

/// The codex provider: holds the upstream base URL, the (live-mutable) request
/// shape (model/fast/effort), and a per-process session id (sent as
/// `session-id` and `prompt_cache_key`, stable so the backend's prompt cache
/// keys stay warm across requests).
///
/// The shape is behind an `RwLock` so the dashboard can toggle fast/model/
/// effort on a running daemon (req8.1) without a restart: requests take a
/// read lock (uncontended on the hot path), the control endpoint takes a write
/// lock.
#[derive(Debug)]
pub struct CodexProvider {
    base_url: String,
    shape: std::sync::RwLock<CodexShape>,
    session_id: String,
}

impl CodexProvider {
    /// Construct with the default request shape (pinned `gpt-5.5`). Used by
    /// tests; production uses [`CodexProvider::with_shape`].
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_shape(base_url, CodexShape::default())
    }

    /// Construct with an explicit request shape (from `config.codex`).
    pub fn with_shape(base_url: impl Into<String>, shape: CodexShape) -> Self {
        Self {
            base_url: base_url.into(),
            shape: std::sync::RwLock::new(shape),
            session_id: uuid_v4(),
        }
    }

    /// Snapshot the current request shape.
    pub fn shape(&self) -> CodexShape {
        self.shape.read().expect("codex shape lock").clone()
    }

    /// Replace the live request shape (dashboard fast/model/effort change).
    pub fn set_shape(&self, shape: CodexShape) {
        *self.shape.write().expect("codex shape lock") = shape;
    }

    /// The model slug this provider currently requests (for the activity log).
    pub fn model(&self) -> String {
        self.shape.read().expect("codex shape lock").model.clone()
    }

    /// The reasoning effort this provider currently sends (for the activity log).
    pub fn effort(&self) -> Option<String> {
        self.shape.read().expect("codex shape lock").effort.clone()
    }

    pub fn endpoint(&self) -> &str {
        &self.base_url
    }

    /// Build the upstream Responses request from an Anthropic Messages body:
    /// translate the body, set the codex header set, inject the credential.
    /// Returns the request plus whether the CLIENT asked for streaming
    /// (upstream is always `stream: true`; non-stream clients get the
    /// aggregated result).
    pub fn build_request(
        &self,
        anthropic_body: &[u8],
        credential: &AccountCredential,
    ) -> Result<(ProviderRequest, bool), ProviderError> {
        let AccountCredential::Codex {
            account_id,
            access_token,
            ..
        } = credential
        else {
            return Err(ProviderError::Auth(
                "codex provider requires a codex credential".into(),
            ));
        };
        let body: Value = serde_json::from_slice(anthropic_body)
            .map_err(|err| ProviderError::Convert(format!("request body is not JSON: {err}")))?;
        let (upstream_body, client_stream) =
            translate_request_with(&body, &self.session_id, &self.shape())?;

        let mut headers = HeaderMap::new();
        let bearer = HeaderValue::from_str(&format!("Bearer {access_token}"))
            .map_err(|err| ProviderError::Auth(err.to_string()))?;
        headers.insert(AUTHORIZATION, bearer);
        headers.insert(
            "chatgpt-account-id",
            HeaderValue::from_str(account_id)
                .map_err(|err| ProviderError::Auth(err.to_string()))?,
        );
        headers.insert(
            "openai-beta",
            HeaderValue::from_static("responses=experimental"),
        );
        headers.insert("originator", HeaderValue::from_static("codex_cli_rs"));
        headers.insert(
            "session-id",
            HeaderValue::from_str(&self.session_id)
                .map_err(|err| ProviderError::Convert(err.to_string()))?,
        );
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        Ok((
            ProviderRequest {
                method: Method::POST,
                path: RESPONSES_PATH.to_string(),
                headers,
                body: Bytes::from(upstream_body.to_string()),
            },
            client_stream,
        ))
    }

    /// Fresh per-request stream converter, stamping responses with this
    /// provider's configured model slug.
    pub fn converter(&self) -> CodexSseConverter {
        CodexSseConverter::with_model(self.shape().model)
    }
}

/// RFC-4122-shaped v4 UUID from the OS CSPRNG (no uuid crate dependency).
fn uuid_v4() -> String {
    let mut bytes = [0u8; 16];
    if let Err(err) = getrandom::fill(&mut bytes) {
        // Same policy as the OAuth PKCE generator: never degrade entropy.
        panic!("OS CSPRNG unavailable: {err}");
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

// ---------------------------------------------------------------------------
// Request translation: Anthropic Messages → Responses API
// ---------------------------------------------------------------------------

/// Translate an Anthropic Messages request body into a Responses API body.
/// Returns `(upstream_body, client_requested_stream)`. The model is ALWAYS
/// rewritten to [`CODEX_MODEL`]; `max_tokens` and `tool_choice` are ignored
/// (logged at debug); images and thinking blocks are dropped (warn/debug).
pub fn translate_request(body: &Value, session_id: &str) -> Result<(Value, bool), ProviderError> {
    translate_request_with(body, session_id, &CodexShape::default())
}

/// Like [`translate_request`] but with an explicit request [`CodexShape`]
/// (configurable model / fast tier / reasoning effort). The model is ALWAYS
/// rewritten to `shape.model`; `max_tokens` and `tool_choice` are ignored
/// (logged at debug); images and thinking blocks are dropped (warn/debug).
/// When `shape.fast`, `service_tier: "priority"` is added (the wire value the
/// codex CLI sends for fast mode); when `shape.effort` is set, a
/// `reasoning: { effort }` object is added.
pub fn translate_request_with(
    body: &Value,
    session_id: &str,
    shape: &CodexShape,
) -> Result<(Value, bool), ProviderError> {
    let client_stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        if model != shape.model {
            tracing::debug!(
                client_model = model,
                "codex: model rewritten to {}",
                shape.model
            );
        }
    }
    if body.get("tool_choice").is_some() {
        tracing::debug!("codex: tool_choice ignored");
    }

    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Convert("request has no messages array".into()))?;
    // The codex `responses` endpoint rejects any `role:"system"` input item
    // ("System messages are not allowed"). Anthropic top-level `system` and any
    // mid-conversation `messages[].role:"system"` (Claude Code's operator /
    // `<system-reminder>` channel) both fold into `instructions` — never into
    // an `input` item. `messages_to_input` returns the legal-role input items
    // plus the text of any system-role messages it skipped.
    let (input, folded_system) = messages_to_input(messages)?;
    let instructions = build_instructions(body.get("system"), &folded_system);
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| tools_to_functions(tools))
        .unwrap_or_default();

    let mut upstream = json!({
        "model": shape.model,
        "instructions": instructions,
        "input": input,
        "tools": tools,
        "parallel_tool_calls": true,
        "store": false,
        "stream": true,
        "prompt_cache_key": session_id,
        "include": ["reasoning.encrypted_content"],
    });
    // Reasoning effort: codex CLI sends `reasoning: { effort }`; omit to keep
    // the backend default. (Empty / "default" effort is treated as unset.)
    if let Some(effort) = shape.effort.as_deref() {
        let effort = effort.trim();
        if !effort.is_empty() && !effort.eq_ignore_ascii_case("default") {
            upstream["reasoning"] = json!({ "effort": effort.to_ascii_lowercase() });
        }
    }
    // Fast mode: codex stores "fast" in config but sends `service_tier:
    // "priority"` on the wire. Only emit the field when fast is on.
    if shape.fast {
        upstream["service_tier"] = json!("priority");
    }
    Ok((upstream, client_stream))
}

/// Anthropic `system` (string or content-block array) → one instruction
/// string.
fn system_text(system: &Value) -> String {
    match system {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Compose the Responses `instructions` string from the Anthropic top-level
/// `system` field plus any system-role messages folded out of `messages[]`.
/// Both are operator instructions, so they concatenate in order (top-level
/// first, then mid-conversation ones as they appeared). This is the only place
/// system content goes — it is never emitted as an `input` item, since codex
/// rejects `role:"system"` items.
fn build_instructions(system: Option<&Value>, folded_system: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(text) = system.map(system_text).filter(|s| !s.is_empty()) {
        parts.push(text);
    }
    parts.extend(folded_system.iter().filter(|s| !s.is_empty()).cloned());
    parts.join("\n")
}

/// The role to stamp on a codex `input` message item. Codex's `responses`
/// endpoint accepts `user`, `assistant`, and `developer`, and rejects
/// `system` ("System messages are not allowed", verified live). Assistant
/// turns map to `assistant`; everything else maps to `user` (system-role
/// messages never reach here — they are folded into `instructions`).
fn input_role(anthropic_role: &str) -> &'static str {
    match anthropic_role {
        "assistant" => "assistant",
        "developer" => "developer",
        _ => "user",
    }
}

/// Anthropic `messages[]` → Responses `input[]` items, plus the text of any
/// `role:"system"` messages (returned separately to fold into `instructions`).
fn messages_to_input(messages: &[Value]) -> Result<(Vec<Value>, Vec<String>), ProviderError> {
    let mut input: Vec<Value> = Vec::new();
    let mut folded_system: Vec<String> = Vec::new();
    for message in messages {
        let anthropic_role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        // System-role messages (Claude Code's mid-conversation operator channel)
        // cannot become input items — codex 400s on `role:"system"`. Pull their
        // text out for `instructions` and emit no input item.
        if anthropic_role == "system" {
            let text = message_text(message);
            if !text.is_empty() {
                folded_system.push(text);
            }
            continue;
        }
        let role = input_role(anthropic_role);
        let text_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        let flush_text = |input: &mut Vec<Value>, text: &mut String| {
            if !text.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": role,
                    "content": [{"type": text_type, "text": std::mem::take(text)}],
                }));
            }
        };
        match message.get("content") {
            Some(Value::String(text)) => {
                let mut text = text.clone();
                flush_text(&mut input, &mut text);
            }
            Some(Value::Array(blocks)) => {
                let mut text = String::new();
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = block.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                        Some("tool_use") => {
                            flush_text(&mut input, &mut text);
                            let arguments = block
                                .get("input")
                                .map(|i| i.to_string())
                                .unwrap_or_else(|| "{}".to_string());
                            input.push(json!({
                                "type": "function_call",
                                "call_id": block.get("id").and_then(Value::as_str).unwrap_or(""),
                                "name": block.get("name").and_then(Value::as_str).unwrap_or(""),
                                "arguments": arguments,
                            }));
                        }
                        Some("tool_result") => {
                            flush_text(&mut input, &mut text);
                            input.push(json!({
                                "type": "function_call_output",
                                "call_id": block
                                    .get("tool_use_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or(""),
                                "output": tool_result_text(block),
                            }));
                        }
                        Some("image") => {
                            tracing::warn!(
                                "codex: image content block dropped (unsupported in v1)"
                            );
                        }
                        // Thinking blocks from a previous codex turn cannot be
                        // replayed upstream — drop them.
                        Some("thinking") | Some("redacted_thinking") => {
                            tracing::debug!("codex: thinking block dropped on request side");
                        }
                        other => {
                            tracing::debug!(block_type = ?other, "codex: unknown content block dropped");
                        }
                    }
                }
                flush_text(&mut input, &mut text);
            }
            _ => {}
        }
    }
    Ok((input, folded_system))
}

/// Plain text of an Anthropic message's `content` (string, or the `text`
/// blocks of a content array joined by newlines). Used to fold a system-role
/// message into `instructions`.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `tool_result.content` (string or text-block array) → plain text output.
fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Anthropic `tools[]` → Responses function tools. Entries without a name
/// (e.g. server-side tool types) are dropped with a debug log.
fn tools_to_functions(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            let mut function = Map::new();
            function.insert("type".into(), json!("function"));
            function.insert("name".into(), json!(name));
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                function.insert("description".into(), json!(description));
            }
            function.insert(
                "parameters".into(),
                tool.get("input_schema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            );
            function.insert("strict".into(), json!(false));
            Some(Value::Object(function))
        })
        .collect()
}

/// Total UTF-8 characters of every string anywhere under `value` (recurses
/// arrays and object values). The atom of the chars/4 token estimate.
fn section_chars(value: &Value) -> u64 {
    match value {
        Value::String(s) => s.chars().count() as u64,
        Value::Array(items) => items.iter().map(section_chars).sum(),
        Value::Object(map) => map.values().map(section_chars).sum(),
        _ => 0,
    }
}

/// chars/4 token estimate for one request section (e.g. just `system`, just
/// `tools`, or just `messages`) so the trace can report the input breakdown
/// per part. NOT floored — sum the parts, then floor the total if needed.
pub fn estimate_section_tokens(value: &Value) -> u64 {
    section_chars(value) / 4
}

/// Naive input-token estimate for `/v1/messages/count_tokens` on a codex
/// account (no upstream equivalent): total characters of system + message
/// text, divided by 4, floor 1.
pub fn estimate_input_tokens(body: &Value) -> u64 {
    let mut total = 0u64;
    if let Some(system) = body.get("system") {
        total += section_chars(system);
    }
    if let Some(messages) = body.get("messages") {
        total += section_chars(messages);
    }
    (total / 4).max(1)
}

// ---------------------------------------------------------------------------
// Response conversion: Responses SSE → Anthropic SSE
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    Text,
    Thinking,
    ToolUse,
}

/// One finished/open content block, kept for the non-stream aggregate.
#[derive(Debug)]
struct AggBlock {
    kind: BlockKind,
    text: String,
    tool_id: String,
    tool_name: String,
    tool_args: String,
}

/// Stateful Responses→Anthropic SSE converter. One instance per upstream
/// response; feed COMPLETE upstream events in, get well-formed Anthropic SSE
/// bytes out (`event: <type>\ndata: <json>\n\n`, indexes sequenced).
#[derive(Debug)]
pub struct CodexSseConverter {
    /// Model slug stamped into the Anthropic `message_start` / aggregate
    /// (what Claude Code sees as the response model).
    model: String,
    started: bool,
    finished: bool,
    message_id: String,
    next_index: usize,
    /// Index of the currently open content block, if any (its kind is the
    /// kind of the LAST entry in `blocks`).
    open_index: Option<usize>,
    saw_tool_use: bool,
    /// `usage.input_tokens` is the FRESH (non-cached) prompt count, matching
    /// Anthropic's convention; the cached subset lives in
    /// `cached_input_tokens`. OpenAI Responses reports the cache-INCLUSIVE
    /// total, so `complete()` subtracts the cached part — otherwise the
    /// dashboard counts cached tokens that the Claude side never counts (≈90×
    /// inflation) and the client's context bar fills on cache reads.
    usage: StreamUsage,
    cached_input_tokens: u64,
    blocks: Vec<AggBlock>,
    stop_reason: Option<String>,
    error: Option<String>,
    /// Verbatim upstream `usage` object from `response.completed`, kept for the
    /// codex trace (input_tokens / input_tokens_details.cached_tokens /
    /// output_tokens / output_tokens_details.reasoning_tokens / total_tokens) —
    /// the reduced `StreamUsage` drops the reasoning + total splits we want to
    /// diagnose token issues from the log.
    raw_usage: Option<Value>,
    /// Count of upstream SSE events parsed (any `data:` event), so the trace
    /// can show whether the stream produced events at all vs. hung.
    events_seen: u64,
}

impl Default for CodexSseConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexSseConverter {
    /// Converter stamping the fallback [`CODEX_MODEL`]. Used by tests; the
    /// provider uses [`CodexSseConverter::with_model`].
    pub fn new() -> Self {
        Self::with_model(CODEX_MODEL.to_string())
    }

    /// Converter that stamps `model` into the synthesized Anthropic response.
    pub fn with_model(model: String) -> Self {
        Self {
            model,
            started: false,
            finished: false,
            message_id: String::new(),
            next_index: 0,
            open_index: None,
            saw_tool_use: false,
            usage: StreamUsage::default(),
            cached_input_tokens: 0,
            blocks: Vec::new(),
            stop_reason: None,
            error: None,
            raw_usage: None,
            events_seen: 0,
        }
    }

    fn emit(out: &mut Vec<u8>, event_type: &str, data: &Value) {
        out.extend_from_slice(format!("event: {event_type}\ndata: {data}\n\n").as_bytes());
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        if self.message_id.is_empty() {
            self.message_id = format!("msg_codex_{}", ulid::Ulid::new().to_string().to_lowercase());
        }
        Self::emit(
            out,
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model.clone(),
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                },
            }),
        );
    }

    fn open_block(&mut self, out: &mut Vec<u8>, kind: BlockKind, content_block: Value) -> usize {
        self.close_block(out);
        let index = self.next_index;
        self.next_index += 1;
        self.open_index = Some(index);
        self.blocks.push(AggBlock {
            kind,
            text: String::new(),
            tool_id: String::new(),
            tool_name: String::new(),
            tool_args: String::new(),
        });
        Self::emit(
            out,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block,
            }),
        );
        index
    }

    fn close_block(&mut self, out: &mut Vec<u8>) {
        if let Some(index) = self.open_index.take() {
            Self::emit(
                out,
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            );
        }
    }

    fn open_kind(&self) -> Option<BlockKind> {
        self.open_index.map(|_| {
            self.blocks
                .last()
                .map(|b| b.kind)
                .unwrap_or(BlockKind::Text)
        })
    }

    fn ensure_block(&mut self, out: &mut Vec<u8>, kind: BlockKind) -> usize {
        if self.open_kind() == Some(kind) {
            return self.open_index.unwrap_or(0);
        }
        let content_block = match kind {
            BlockKind::Text => json!({"type": "text", "text": ""}),
            BlockKind::Thinking => json!({"type": "thinking", "thinking": ""}),
            // Tool blocks are only ever opened explicitly with id+name.
            BlockKind::ToolUse => json!({"type": "tool_use", "id": "", "name": "", "input": {}}),
        };
        self.open_block(out, kind, content_block)
    }

    fn delta(&mut self, out: &mut Vec<u8>, index: usize, delta: Value) {
        Self::emit(
            out,
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": index, "delta": delta}),
        );
    }

    fn fail(&mut self, out: &mut Vec<u8>, message: &str) {
        if self.finished {
            return;
        }
        self.finished = true;
        self.error = Some(message.to_string());
        Self::emit(
            out,
            "error",
            &json!({
                "type": "error",
                "error": {"type": "api_error", "message": message},
            }),
        );
    }

    fn complete(&mut self, out: &mut Vec<u8>, response: Option<&Value>) {
        if self.finished {
            return;
        }
        self.ensure_started(out);
        self.close_block(out);
        if let Some(usage) = response.and_then(|r| r.get("usage")) {
            // Keep the verbatim upstream usage for the codex trace before we
            // reduce it (the trace wants reasoning + total splits too).
            self.raw_usage = Some(usage.clone());
            // OpenAI `input_tokens` is the cache-INCLUSIVE total; the cached
            // subset is `input_tokens_details.cached_tokens`. Record fresh =
            // total − cached so codex is comparable to the Anthropic side
            // (which already counts uncached input only).
            let total_input = usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            // `cached` is `Some` only when the upstream reported the field, so
            // the dashboard renders unavailable (not 0) when it is absent.
            let cached = usage
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(Value::as_u64);
            self.cached_input_tokens = cached.unwrap_or(0);
            self.usage = StreamUsage {
                input_tokens: total_input.saturating_sub(self.cached_input_tokens),
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
                cache_read_input_tokens: cached,
                // OpenAI Responses does not report cache-creation tokens.
                cache_creation_input_tokens: None,
            };
        }
        let stop_reason = if self.saw_tool_use {
            "tool_use"
        } else {
            "end_turn"
        };
        self.stop_reason = Some(stop_reason.to_string());
        Self::emit(
            out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {
                    "input_tokens": self.usage.input_tokens,
                    "cache_read_input_tokens": self.cached_input_tokens,
                    "output_tokens": self.usage.output_tokens,
                },
            }),
        );
        Self::emit(out, "message_stop", &json!({"type": "message_stop"}));
        self.finished = true;
    }

    /// Track aggregate content for the non-stream response.
    fn aggregate(&mut self, kind: BlockKind, push: impl FnOnce(&mut AggBlock)) {
        if let Some(block) = self.blocks.last_mut() {
            if block.kind == kind {
                push(block);
            }
        }
    }

    /// Build the single (non-streaming) Anthropic Messages response from the
    /// fully consumed stream. `None` when the upstream reported an error —
    /// callers should surface [`Self::error_message`] instead.
    pub fn into_message_json(self) -> Option<Value> {
        if self.error.is_some() {
            return None;
        }
        let content: Vec<Value> = self
            .blocks
            .iter()
            .map(|block| match block.kind {
                BlockKind::Text => json!({"type": "text", "text": block.text}),
                BlockKind::Thinking => json!({"type": "thinking", "thinking": block.text}),
                BlockKind::ToolUse => json!({
                    "type": "tool_use",
                    "id": block.tool_id,
                    "name": block.tool_name,
                    "input": serde_json::from_str::<Value>(&block.tool_args)
                        .unwrap_or_else(|_| json!({})),
                }),
            })
            .collect();
        Some(json!({
            "id": if self.message_id.is_empty() {
                format!("msg_codex_{}", ulid::Ulid::new().to_string().to_lowercase())
            } else {
                self.message_id.clone()
            },
            "type": "message",
            "role": "assistant",
            "model": self.model.clone(),
            "content": content,
            "stop_reason": self.stop_reason.as_deref().unwrap_or("end_turn"),
            "stop_sequence": null,
            "usage": {
                "input_tokens": self.usage.input_tokens,
                "cache_read_input_tokens": self.cached_input_tokens,
                "output_tokens": self.usage.output_tokens,
            },
        }))
    }

    /// Upstream error message, when the stream ended in `response.failed` /
    /// `error`.
    pub fn error_message(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Verbatim upstream `usage` object captured at `response.completed`, for
    /// the codex trace. `None` until a `response.completed` carrying `usage`
    /// has been folded.
    pub fn raw_usage(&self) -> Option<&Value> {
        self.raw_usage.as_ref()
    }

    /// Count of real upstream SSE events parsed so far (keepalives, `[DONE]`,
    /// and unparseable lines excluded).
    pub fn events_seen(&self) -> u64 {
        self.events_seen
    }

    /// Concatenated `data:` payload of one SSE event (Responses events are
    /// single-line JSON in practice; multi-line data is joined per the SSE
    /// spec).
    fn event_data(event: &str) -> Option<String> {
        let lines: Vec<&str> = event
            .lines()
            .filter_map(|line| {
                line.strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
            })
            .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }
}

impl SseTransform for CodexSseConverter {
    fn on_event(&mut self, event: &str) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let Some(data) = Self::event_data(event) else {
            return out; // comment/keepalive lines
        };
        if data.trim() == "[DONE]" {
            return out;
        }
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            tracing::debug!("codex: unparseable upstream SSE data dropped");
            return out;
        };
        // One real upstream event parsed (keepalives / [DONE] / unparseable
        // lines already returned above) — surfaced in the codex trace.
        self.events_seen += 1;
        let event_type = value
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                event
                    .lines()
                    .find_map(|l| {
                        l.strip_prefix("event: ")
                            .or_else(|| l.strip_prefix("event:"))
                    })
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_default();

        match event_type.as_str() {
            "response.created" => {
                if let Some(id) = value
                    .get("response")
                    .and_then(|r| r.get("id"))
                    .and_then(Value::as_str)
                {
                    self.message_id = id.to_string();
                }
                self.ensure_started(&mut out);
            }
            "response.output_item.added" => {
                self.ensure_started(&mut out);
                let item = value.get("item");
                match item.and_then(|i| i.get("type")).and_then(Value::as_str) {
                    Some("message") => {
                        self.ensure_block(&mut out, BlockKind::Text);
                    }
                    Some("function_call") => {
                        let call_id = item
                            .and_then(|i| i.get("call_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = item
                            .and_then(|i| i.get("name"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        self.saw_tool_use = true;
                        self.open_block(
                            &mut out,
                            BlockKind::ToolUse,
                            json!({"type": "tool_use", "id": call_id, "name": name, "input": {}}),
                        );
                        if let Some(block) = self.blocks.last_mut() {
                            block.tool_id = call_id;
                            block.tool_name = name;
                        }
                    }
                    _ => {} // reasoning items etc. open lazily via their deltas
                }
            }
            "response.output_text.delta" => {
                self.ensure_started(&mut out);
                let index = self.ensure_block(&mut out, BlockKind::Text);
                let text = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.delta(&mut out, index, json!({"type": "text_delta", "text": text}));
                self.aggregate(BlockKind::Text, |b| b.text.push_str(&text));
            }
            "response.reasoning_summary_text.delta" => {
                self.ensure_started(&mut out);
                let index = self.ensure_block(&mut out, BlockKind::Thinking);
                let text = value
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                self.delta(
                    &mut out,
                    index,
                    json!({"type": "thinking_delta", "thinking": text}),
                );
                self.aggregate(BlockKind::Thinking, |b| b.text.push_str(&text));
            }
            "response.function_call_arguments.delta" => {
                if self.open_kind() == Some(BlockKind::ToolUse) {
                    let index = self.open_index.unwrap_or(0);
                    let partial = value
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    self.delta(
                        &mut out,
                        index,
                        json!({"type": "input_json_delta", "partial_json": partial}),
                    );
                    self.aggregate(BlockKind::ToolUse, |b| b.tool_args.push_str(&partial));
                }
            }
            "response.output_item.done" => {
                // A function_call item may deliver its full arguments only
                // here (no deltas streamed) — emit them before closing.
                if self.open_kind() == Some(BlockKind::ToolUse) {
                    let streamed = self
                        .blocks
                        .last()
                        .map(|b| !b.tool_args.is_empty())
                        .unwrap_or(false);
                    if !streamed {
                        if let Some(arguments) = value
                            .get("item")
                            .and_then(|i| i.get("arguments"))
                            .and_then(Value::as_str)
                            .filter(|a| !a.is_empty())
                        {
                            let index = self.open_index.unwrap_or(0);
                            let arguments = arguments.to_string();
                            self.delta(
                                &mut out,
                                index,
                                json!({"type": "input_json_delta", "partial_json": arguments}),
                            );
                            self.aggregate(BlockKind::ToolUse, |b| {
                                b.tool_args.push_str(&arguments)
                            });
                        }
                    }
                }
                self.close_block(&mut out);
            }
            "response.completed" => {
                self.complete(&mut out, value.get("response"));
            }
            "response.failed" => {
                let message = value
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .and_then(|e| e.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("upstream response failed")
                    .to_string();
                self.fail(&mut out, &message);
            }
            "error" => {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        value
                            .get("error")
                            .and_then(|e| e.get("message"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("upstream error")
                    .to_string();
                self.fail(&mut out, &message);
            }
            // in_progress / content_part / output_text.done / reasoning
            // bookkeeping events carry nothing the Anthropic stream needs.
            _ => {}
        }
        out
    }

    fn on_end(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.finished {
            // Never-started covers a 2xx whose body was not SSE at all
            // (e.g. a plain JSON document): relay_codex trusts every 2xx to
            // be a stream, so the converter must terminate it with a clean
            // Anthropic error event rather than ending silently.
            let message = if self.started {
                "upstream stream ended before response.completed"
            } else {
                "codex upstream returned no SSE events"
            };
            self.fail(&mut out, message);
        }
        out
    }

    fn usage(&self) -> StreamUsage {
        self.usage
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codex_credential() -> AccountCredential {
        AccountCredential::Codex {
            account_id: "acct-1".into(),
            access_token: "at-codex".into(),
            refresh_token: "rt-codex".into(),
            expires_at_ms: u64::MAX,
            last_refresh_ms: None,
        }
    }

    // ---- request translation ----

    #[test]
    fn translate_simple_text_request() {
        let body = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "stream": true,
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
                {"role": "user", "content": [{"type": "text", "text": "again"}]}
            ]
        });
        let (upstream, client_stream) = translate_request(&body, "sess-1").expect("translate");
        assert!(client_stream);
        assert_eq!(upstream["model"], CODEX_MODEL, "model always rewritten");
        assert_eq!(upstream["instructions"], "You are helpful.");
        assert_eq!(upstream["stream"], true);
        assert_eq!(upstream["store"], false);
        assert_eq!(upstream["parallel_tool_calls"], true);
        assert_eq!(upstream["prompt_cache_key"], "sess-1");
        assert_eq!(upstream["include"][0], "reasoning.encrypted_content");
        let input = upstream["input"].as_array().expect("input");
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "hello");
        assert_eq!(input[2]["content"][0]["text"], "again");
    }

    #[test]
    fn shape_sets_configurable_model_fast_tier_and_effort() {
        let body = json!({ "model": "gpt-5.5", "messages": [{"role":"user","content":"hi"}] });
        let shape = CodexShape {
            model: "gpt-5.5-codex".to_string(),
            fast: true,
            effort: Some("XHIGH".to_string()),
        };
        let (upstream, _) = translate_request_with(&body, "s", &shape).expect("translate");
        assert_eq!(upstream["model"], "gpt-5.5-codex", "model is config-driven");
        // codex stores "fast" but sends service_tier=priority on the wire.
        assert_eq!(upstream["service_tier"], "priority");
        // effort lowercased into reasoning.effort.
        assert_eq!(upstream["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn shape_default_omits_tier_and_reasoning() {
        let body = json!({ "model": "gpt-5.5", "messages": [{"role":"user","content":"hi"}] });
        let (upstream, _) =
            translate_request_with(&body, "s", &CodexShape::default()).expect("translate");
        assert_eq!(upstream["model"], CODEX_MODEL);
        assert!(
            upstream.get("service_tier").is_none(),
            "no tier when not fast"
        );
        assert!(upstream.get("reasoning").is_none(), "no effort by default");
    }

    #[test]
    fn shape_treats_blank_or_default_effort_as_unset() {
        let body = json!({ "model": "x", "messages": [{"role":"user","content":"hi"}] });
        for e in ["", "  ", "default", "DEFAULT"] {
            let shape = CodexShape {
                model: CODEX_MODEL.to_string(),
                fast: false,
                effort: Some(e.to_string()),
            };
            let (upstream, _) = translate_request_with(&body, "s", &shape).expect("translate");
            assert!(
                upstream.get("reasoning").is_none(),
                "effort {e:?} should be treated as unset"
            );
        }
    }

    #[test]
    fn translate_system_blocks_join_to_instructions() {
        let body = json!({
            "system": [
                {"type": "text", "text": "Line one."},
                {"type": "text", "text": "Line two."}
            ],
            "messages": [{"role": "user", "content": "x"}]
        });
        let (upstream, client_stream) = translate_request(&body, "s").expect("translate");
        assert!(!client_stream, "stream defaults to false");
        assert_eq!(upstream["instructions"], "Line one.\nLine two.");
    }

    /// The codex `responses` endpoint rejects any `role:"system"` input item
    /// ("System messages are not allowed", verified live on :3477). A bare
    /// system-role message (the original P3 repro shape) must NOT become an
    /// input item — its text folds into `instructions` instead.
    #[test]
    fn translate_system_role_message_never_becomes_input_item() {
        let body = json!({
            "max_tokens": 40,
            "stream": true,
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "say OK"}
            ]
        });
        let (upstream, _) = translate_request(&body, "s").expect("translate");
        let input = upstream["input"].as_array().expect("input");
        // The system message produced no input item; only the user message did.
        assert_eq!(input.len(), 1, "system message must not emit an input item");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "say OK");
        // No input item may carry role:"system" (the codex 400 trigger).
        for item in input {
            assert_ne!(
                item["role"], "system",
                "no input item may have role:\"system\""
            );
        }
        // The system text is preserved as an instruction, not dropped.
        assert_eq!(upstream["instructions"], "be brief");
    }

    /// The exact shape Claude Code emits via the mid-conversation system beta
    /// (`mid-conversation-system-2026-04-07`): user → assistant → system → user.
    /// This was the live 400 repro; it must now translate cleanly with the
    /// mid-conversation system text folded after the top-level system prompt
    /// and zero `role:"system"` input items.
    #[test]
    fn translate_mid_conversation_system_message_folds_into_instructions() {
        let body = json!({
            "system": "You are helpful.",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
                {"role": "system", "content": "Terse mode enabled."},
                {"role": "user", "content": "say OK"}
            ]
        });
        let (upstream, _) = translate_request(&body, "s").expect("translate");
        let input = upstream["input"].as_array().expect("input");
        assert_eq!(input.len(), 3, "user, assistant, user — system folded out");
        let roles: Vec<&str> = input
            .iter()
            .filter(|i| i["type"] == "message")
            .map(|i| i["role"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
        assert!(
            input.iter().all(|i| i["role"] != "system"),
            "no role:\"system\" input item may survive"
        );
        // Top-level system first, mid-conversation system appended after it.
        assert_eq!(
            upstream["instructions"],
            "You are helpful.\nTerse mode enabled."
        );
    }

    /// Codex accepts `role:"developer"` (verified live: 200). A developer-role
    /// message passes through as a `developer` input item, not coerced to user
    /// and never to system.
    #[test]
    fn translate_developer_role_passes_through() {
        let body = json!({
            "messages": [
                {"role": "developer", "content": "be brief"},
                {"role": "user", "content": "say OK"}
            ]
        });
        let (upstream, _) = translate_request(&body, "s").expect("translate");
        let input = upstream["input"].as_array().expect("input");
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "be brief");
        assert_eq!(input[1]["role"], "user");
    }

    /// Any unrecognized role degrades to `user` — never to `system` (the one
    /// role codex forbids).
    #[test]
    fn translate_unknown_role_degrades_to_user_not_system() {
        let body = json!({
            "messages": [{"role": "tool", "content": "result text"}]
        });
        let (upstream, _) = translate_request(&body, "s").expect("translate");
        let input = upstream["input"].as_array().expect("input");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user", "unknown role → user");
    }

    /// A system-role message expressed as a content-block array (Anthropic's
    /// other system shape) also folds into instructions, joining block text.
    #[test]
    fn translate_system_role_message_with_block_content_folds() {
        let body = json!({
            "messages": [
                {"role": "system", "content": [
                    {"type": "text", "text": "Rule one."},
                    {"type": "text", "text": "Rule two."}
                ]},
                {"role": "user", "content": "go"}
            ]
        });
        let (upstream, _) = translate_request(&body, "s").expect("translate");
        assert_eq!(upstream["instructions"], "Rule one.\nRule two.");
        let input = upstream["input"].as_array().expect("input");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn translate_tool_use_and_tool_result_round() {
        let body = json!({
            "messages": [
                {"role": "user", "content": "weather?"},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "checking"},
                    {"type": "tool_use", "id": "call_1", "name": "get_weather",
                     "input": {"city": "Seoul"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1",
                     "content": [{"type": "text", "text": "22C"}]}
                ]}
            ],
            "tools": [
                {"name": "get_weather", "description": "Get weather",
                 "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}}}
            ],
            "tool_choice": {"type": "auto"}
        });
        let (upstream, _) = translate_request(&body, "s").expect("translate");
        let input = upstream["input"].as_array().expect("input");
        assert_eq!(input.len(), 4, "user text, assistant text, call, output");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["text"], "checking");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["name"], "get_weather");
        let args: Value =
            serde_json::from_str(input[2]["arguments"].as_str().expect("args string"))
                .expect("args json");
        assert_eq!(args["city"], "Seoul");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_1");
        assert_eq!(input[3]["output"], "22C");

        let tools = upstream["tools"].as_array().expect("tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["description"], "Get weather");
        assert_eq!(tools[0]["strict"], false);
        assert_eq!(tools[0]["parameters"]["type"], "object");
        assert!(upstream.get("tool_choice").is_none(), "tool_choice ignored");
    }

    #[test]
    fn translate_drops_images_and_thinking() {
        let body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "image", "source": {"type": "base64", "data": "..."}},
                    {"type": "text", "text": "what is this"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "text", "text": "a cat"}
                ]}
            ]
        });
        let (upstream, _) = translate_request(&body, "s").expect("translate");
        let input = upstream["input"].as_array().expect("input");
        assert_eq!(input.len(), 2, "image and thinking dropped");
        assert_eq!(input[0]["content"][0]["text"], "what is this");
        assert_eq!(input[1]["content"][0]["text"], "a cat");
    }

    #[test]
    fn translate_rejects_missing_messages() {
        assert!(translate_request(&json!({"model": "m"}), "s").is_err());
    }

    #[test]
    fn build_request_sets_codex_headers() {
        let provider = CodexProvider::new("https://chatgpt.example/backend-api/codex");
        let body = json!({"stream": true, "messages": [{"role": "user", "content": "hi"}]});
        let (req, client_stream) = provider
            .build_request(body.to_string().as_bytes(), &codex_credential())
            .expect("build");
        assert!(client_stream);
        assert_eq!(req.method, Method::POST);
        assert_eq!(req.path, "/responses");
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer at-codex");
        assert_eq!(req.headers.get("chatgpt-account-id").unwrap(), "acct-1");
        assert_eq!(
            req.headers.get("openai-beta").unwrap(),
            "responses=experimental"
        );
        assert_eq!(req.headers.get("originator").unwrap(), "codex_cli_rs");
        assert_eq!(req.headers.get("accept").unwrap(), "text/event-stream");
        let session = req
            .headers
            .get("session-id")
            .and_then(|v| v.to_str().ok())
            .expect("session-id");
        assert_eq!(session.len(), 36, "uuid shape");
        let sent: Value = serde_json::from_slice(&req.body).expect("json");
        assert_eq!(sent["model"], CODEX_MODEL);
        assert_eq!(sent["prompt_cache_key"], session, "cache key = session id");
    }

    #[test]
    fn build_request_refuses_non_codex_credentials() {
        let provider = CodexProvider::new("https://x");
        let err = provider
            .build_request(
                br#"{"messages":[]}"#,
                &AccountCredential::Apikey {
                    api_key: "sk".into(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, ProviderError::Auth(_)));
    }

    #[test]
    fn estimate_tokens_is_roughly_chars_over_four() {
        let body = json!({
            "system": "abcd",
            "messages": [{"role": "user", "content": "efghijkl"}]
        });
        // "user" (4) + content (8) + system (4) = 16 chars → 4 tokens.
        assert_eq!(estimate_input_tokens(&body), 4);
        assert_eq!(estimate_input_tokens(&json!({})), 1, "floor of 1");
    }

    // ---- SSE converter ----

    fn event(json: Value) -> String {
        format!(
            "event: {}\ndata: {json}",
            json["type"].as_str().unwrap_or("message")
        )
    }

    /// Feed a scripted sequence and split the emitted bytes back into
    /// `(event_type, data_json)` pairs for assertion.
    fn run_converter(events: &[Value]) -> (CodexSseConverter, Vec<(String, Value)>) {
        let mut converter = CodexSseConverter::new();
        let mut emitted = Vec::new();
        for e in events {
            emitted.extend_from_slice(&converter.on_event(&event(e.clone())));
        }
        emitted.extend_from_slice(&converter.on_end());
        let text = String::from_utf8(emitted).expect("utf8");
        let mut parsed = Vec::new();
        for chunk in text.split("\n\n").filter(|c| !c.trim().is_empty()) {
            let mut event_type = String::new();
            let mut data = String::new();
            for line in chunk.lines() {
                if let Some(t) = line.strip_prefix("event: ") {
                    event_type = t.to_string();
                } else if let Some(d) = line.strip_prefix("data: ") {
                    data = d.to_string();
                } else {
                    panic!("malformed SSE line: {line:?}");
                }
            }
            assert!(!event_type.is_empty(), "every event needs an event: line");
            let value: Value = serde_json::from_str(&data).expect("data is json");
            assert_eq!(
                value["type"], event_type,
                "data.type must match the event line"
            );
            parsed.push((event_type, value));
        }
        (converter, parsed)
    }

    fn types(events: &[(String, Value)]) -> Vec<&str> {
        events.iter().map(|(t, _)| t.as_str()).collect()
    }

    #[test]
    fn text_only_stream_maps_to_anthropic_sequence() {
        let (converter, events) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "resp_1"}}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "message", "role": "assistant"}}),
            json!({"type": "response.output_text.delta", "delta": "Hel"}),
            json!({"type": "response.output_text.delta", "delta": "lo"}),
            json!({"type": "response.output_item.done", "item": {"type": "message"}}),
            json!({"type": "response.completed",
                   "response": {"usage": {"input_tokens": 12, "output_tokens": 5}}}),
        ]);
        assert_eq!(
            types(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let (_, start) = &events[0];
        assert_eq!(start["message"]["id"], "resp_1");
        assert_eq!(start["message"]["model"], CODEX_MODEL);
        let (_, block_start) = &events[1];
        assert_eq!(block_start["index"], 0);
        assert_eq!(block_start["content_block"]["type"], "text");
        assert_eq!(events[2].1["delta"]["text"], "Hel");
        assert_eq!(events[3].1["delta"]["text"], "lo");
        assert_eq!(events[4].1["index"], 0);
        let (_, message_delta) = &events[5];
        assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(message_delta["usage"]["input_tokens"], 12);
        assert_eq!(message_delta["usage"]["output_tokens"], 5);
        assert_eq!(
            converter.usage(),
            StreamUsage {
                input_tokens: 12,
                output_tokens: 5,
                // No cached_tokens key in the payload → unavailable, not zero.
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
            }
        );
    }

    #[test]
    fn cached_input_is_excluded_from_fresh_and_emitted_as_cache_read() {
        // OpenAI reports the cache-INCLUSIVE total in `input_tokens` with the
        // cached subset in `input_tokens_details.cached_tokens`. Record fresh =
        // total − cached (comparable to the Anthropic side, which counts
        // uncached input only) and surface the cached part as
        // `cache_read_input_tokens` so the client's context bar doesn't fill on
        // cache reads. Regression for the ~90× codex token inflation.
        let (converter, events) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"type": "message", "role": "assistant"}}),
            json!({"type": "response.output_text.delta", "delta": "hi"}),
            json!({"type": "response.output_item.done", "item": {"type": "message"}}),
            json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 200_000,
                "input_tokens_details": {"cached_tokens": 199_000},
                "output_tokens": 42
            }}}),
        ]);
        let (_, message_delta) = events.iter().find(|(t, _)| t == "message_delta").unwrap();
        assert_eq!(
            message_delta["usage"]["input_tokens"], 1_000,
            "fresh = 200000 - 199000"
        );
        assert_eq!(message_delta["usage"]["cache_read_input_tokens"], 199_000);
        assert_eq!(message_delta["usage"]["output_tokens"], 42);
        // Dashboard totals read converter.usage(): fresh only, cached surfaced.
        assert_eq!(
            converter.usage(),
            StreamUsage {
                input_tokens: 1_000,
                output_tokens: 42,
                cache_read_input_tokens: Some(199_000),
                cache_creation_input_tokens: None,
            }
        );
    }

    #[test]
    fn tool_call_stream_emits_tool_use_block_and_stop_reason() {
        let (_, events) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "resp_2"}}),
            json!({"type": "response.output_item.added",
                   "item": {"type": "message", "role": "assistant"}}),
            json!({"type": "response.output_text.delta", "delta": "checking"}),
            json!({"type": "response.output_item.done", "item": {"type": "message"}}),
            json!({"type": "response.output_item.added",
                   "item": {"type": "function_call", "call_id": "call_9",
                            "name": "get_weather", "arguments": ""}}),
            json!({"type": "response.function_call_arguments.delta", "delta": "{\"city\":"}),
            json!({"type": "response.function_call_arguments.delta", "delta": "\"Seoul\"}"}),
            json!({"type": "response.output_item.done",
                   "item": {"type": "function_call", "call_id": "call_9",
                            "name": "get_weather", "arguments": "{\"city\":\"Seoul\"}"}}),
            json!({"type": "response.completed",
                   "response": {"usage": {"input_tokens": 30, "output_tokens": 9}}}),
        ]);
        assert_eq!(
            types(&events),
            vec![
                "message_start",
                "content_block_start", // text
                "content_block_delta",
                "content_block_stop",
                "content_block_start", // tool_use
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let (_, tool_start) = &events[4];
        assert_eq!(tool_start["index"], 1, "indexes sequence 0, 1");
        assert_eq!(tool_start["content_block"]["type"], "tool_use");
        assert_eq!(tool_start["content_block"]["id"], "call_9");
        assert_eq!(tool_start["content_block"]["name"], "get_weather");
        assert_eq!(events[5].1["delta"]["type"], "input_json_delta");
        assert_eq!(events[5].1["delta"]["partial_json"], "{\"city\":");
        assert_eq!(events[6].1["delta"]["partial_json"], "\"Seoul\"}");
        assert_eq!(events[7].1["index"], 1);
        assert_eq!(events[8].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn tool_arguments_only_at_item_done_still_emit_one_delta() {
        let (_, events) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.output_item.added",
                   "item": {"type": "function_call", "call_id": "c1", "name": "f"}}),
            json!({"type": "response.output_item.done",
                   "item": {"type": "function_call", "call_id": "c1", "name": "f",
                            "arguments": "{\"a\":1}"}}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
        ]);
        assert_eq!(
            types(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[2].1["delta"]["partial_json"], "{\"a\":1}");
    }

    #[test]
    fn reasoning_deltas_open_and_close_a_thinking_block() {
        let (_, events) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "let me think"}),
            json!({"type": "response.output_item.added",
                   "item": {"type": "message", "role": "assistant"}}),
            json!({"type": "response.output_text.delta", "delta": "answer"}),
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 2, "output_tokens": 2}}}),
        ]);
        assert_eq!(
            types(&events),
            vec![
                "message_start",
                "content_block_start", // thinking (index 0)
                "content_block_delta",
                "content_block_stop",  // thinking closed when text starts
                "content_block_start", // text (index 1)
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[1].1["content_block"]["type"], "thinking");
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[2].1["delta"]["type"], "thinking_delta");
        assert_eq!(events[2].1["delta"]["thinking"], "let me think");
        assert_eq!(events[4].1["content_block"]["type"], "text");
        assert_eq!(events[4].1["index"], 1);
    }

    #[test]
    fn upstream_error_event_maps_to_anthropic_error_and_terminates() {
        let (converter, events) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "error", "message": "quota exceeded"}),
            // Anything after the error must be swallowed.
            json!({"type": "response.output_text.delta", "delta": "x"}),
        ]);
        assert_eq!(types(&events), vec!["message_start", "error"]);
        assert_eq!(events[1].1["error"]["type"], "api_error");
        assert_eq!(events[1].1["error"]["message"], "quota exceeded");
        assert_eq!(converter.error_message(), Some("quota exceeded"));
    }

    #[test]
    fn response_failed_maps_to_error() {
        let (_, events) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.failed",
                   "response": {"error": {"message": "server melted"}}}),
        ]);
        assert_eq!(types(&events), vec!["message_start", "error"]);
        assert_eq!(events[1].1["error"]["message"], "server melted");
    }

    #[test]
    fn truncated_stream_emits_error_on_end() {
        let (_, events) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.output_text.delta", "delta": "hal"}),
        ]);
        let kinds = types(&events);
        assert_eq!(
            kinds.last(),
            Some(&"error"),
            "missing response.completed must not look like a clean end: {kinds:?}"
        );
    }

    #[test]
    fn aggregate_builds_non_streaming_message_json() {
        let (converter, _) = run_converter(&[
            json!({"type": "response.created", "response": {"id": "resp_agg"}}),
            json!({"type": "response.output_item.added",
                   "item": {"type": "message", "role": "assistant"}}),
            json!({"type": "response.output_text.delta", "delta": "The answer "}),
            json!({"type": "response.output_text.delta", "delta": "is 42."}),
            json!({"type": "response.output_item.done", "item": {"type": "message"}}),
            json!({"type": "response.output_item.added",
                   "item": {"type": "function_call", "call_id": "c2", "name": "save"}}),
            json!({"type": "response.function_call_arguments.delta", "delta": "{\"v\":42}"}),
            json!({"type": "response.output_item.done", "item": {"type": "function_call"}}),
            json!({"type": "response.completed",
                   "response": {"usage": {"input_tokens": 7, "output_tokens": 3}}}),
        ]);
        let message = converter.into_message_json().expect("message");
        assert_eq!(message["id"], "resp_agg");
        assert_eq!(message["model"], CODEX_MODEL);
        assert_eq!(message["stop_reason"], "tool_use");
        assert_eq!(message["content"][0]["type"], "text");
        assert_eq!(message["content"][0]["text"], "The answer is 42.");
        assert_eq!(message["content"][1]["type"], "tool_use");
        assert_eq!(message["content"][1]["id"], "c2");
        assert_eq!(message["content"][1]["name"], "save");
        assert_eq!(message["content"][1]["input"]["v"], 42);
        assert_eq!(message["usage"]["input_tokens"], 7);
        assert_eq!(message["usage"]["output_tokens"], 3);
    }

    #[test]
    fn aggregate_of_failed_stream_is_none() {
        let (converter, _) = run_converter(&[json!({"type": "error", "message": "nope"})]);
        assert!(converter.into_message_json().is_none());
    }

    #[test]
    fn live_captured_sequence_maps_to_clean_anthropic_stream() {
        // Event shapes from the 2026-06-12 live chatgpt.com smoke capture:
        // a reasoning item with encrypted_content and an EMPTY summary (no
        // reasoning_summary_text.delta ever fires), a message item tagged
        // phase:"final_answer", obfuscation fields on every text delta, and
        // the in_progress / content_part.* / output_text.done bookkeeping
        // events Phase C's scripted tests never exercised. None of the
        // ignorable events may produce malformed or stray blocks.
        let (converter, events) = run_converter(&[
            json!({"type": "response.created",
                   "response": {"id": "resp_live", "object": "response",
                                "status": "in_progress", "model": "gpt-5.5",
                                "output": [], "usage": null}}),
            json!({"type": "response.in_progress",
                   "response": {"id": "resp_live", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0,
                   "item": {"id": "rs_live_1", "type": "reasoning",
                            "encrypted_content": "gAAAAA-opaque", "summary": []}}),
            json!({"type": "response.output_item.done", "output_index": 0,
                   "item": {"id": "rs_live_1", "type": "reasoning",
                            "encrypted_content": "gAAAAA-opaque", "summary": []}}),
            json!({"type": "response.output_item.added", "output_index": 1,
                   "item": {"id": "msg_live_1", "type": "message",
                            "status": "in_progress", "content": [],
                            "phase": "final_answer", "role": "assistant"}}),
            json!({"type": "response.content_part.added", "content_index": 0,
                   "item_id": "msg_live_1", "output_index": 1,
                   "part": {"type": "output_text", "annotations": [],
                            "logprobs": [], "text": ""}}),
            json!({"type": "response.output_text.delta", "content_index": 0,
                   "delta": "O", "item_id": "msg_live_1", "logprobs": [],
                   "obfuscation": "ydFpcUg7ZI1oyX", "output_index": 1}),
            json!({"type": "response.output_text.delta", "content_index": 0,
                   "delta": "K", "item_id": "msg_live_1", "logprobs": [],
                   "obfuscation": "x91js", "output_index": 1}),
            json!({"type": "response.output_text.delta", "content_index": 0,
                   "delta": ", ", "item_id": "msg_live_1", "logprobs": [],
                   "obfuscation": "p2", "output_index": 1}),
            json!({"type": "response.output_text.delta", "content_index": 0,
                   "delta": "done", "item_id": "msg_live_1", "logprobs": [],
                   "obfuscation": "qq8", "output_index": 1}),
            json!({"type": "response.output_text.done", "content_index": 0,
                   "item_id": "msg_live_1", "logprobs": [], "output_index": 1,
                   "text": "OK, done"}),
            json!({"type": "response.content_part.done", "content_index": 0,
                   "item_id": "msg_live_1", "output_index": 1,
                   "part": {"type": "output_text", "annotations": [],
                            "logprobs": [], "text": "OK, done"}}),
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"id": "msg_live_1", "type": "message",
                            "status": "completed",
                            "content": [{"type": "output_text", "text": "OK, done"}],
                            "phase": "final_answer", "role": "assistant"}}),
            json!({"type": "response.completed",
                   "response": {"id": "resp_live", "status": "completed",
                                "usage": {"input_tokens": 8,
                                          "input_tokens_details": {"cached_tokens": 0},
                                          "output_tokens": 5,
                                          "total_tokens": 13}}}),
        ]);
        assert_eq!(
            types(&events),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[0].1["message"]["id"], "resp_live");
        assert_eq!(events[1].1["content_block"]["type"], "text");
        assert_eq!(
            events[1].1["index"], 0,
            "reasoning item must not burn an index"
        );
        for (i, expected) in [(2, "O"), (3, "K"), (4, ", "), (5, "done")] {
            assert_eq!(events[i].1["delta"]["type"], "text_delta");
            assert_eq!(events[i].1["delta"]["text"], expected);
        }
        assert_eq!(events[7].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[7].1["usage"]["input_tokens"], 8);
        assert_eq!(events[7].1["usage"]["output_tokens"], 5);
        assert!(converter.error_message().is_none());
        assert_eq!(
            converter.usage(),
            StreamUsage {
                input_tokens: 8,
                output_tokens: 5,
                // The live payload reports input_tokens_details.cached_tokens=0,
                // so cache-read is an explicit Some(0), not unavailable.
                cache_read_input_tokens: Some(0),
                cache_creation_input_tokens: None,
            }
        );
    }

    #[test]
    fn json_body_instead_of_sse_terminates_with_clean_error_event() {
        // relay_codex trusts every 2xx to be SSE; a plain JSON body yields
        // zero parseable events (the EventBuffer hands the whole document
        // over as one terminal remainder with no `data:` lines). The
        // converter must end the client stream with a clean Anthropic error
        // event — never silence, never garbage.
        let mut converter = CodexSseConverter::new();
        assert!(
            converter
                .on_event(r#"{"detail":"not an event stream"}"#)
                .is_empty(),
            "a non-SSE body produces no downstream events"
        );
        let out = converter.on_end();
        let text = String::from_utf8(out).expect("utf8");
        let chunks: Vec<&str> = text
            .split("\n\n")
            .filter(|c| !c.trim().is_empty())
            .collect();
        assert_eq!(chunks.len(), 1, "exactly one terminal event: {text:?}");
        assert!(chunks[0].starts_with("event: error\n"), "{text:?}");
        let data: Value = serde_json::from_str(
            chunks[0]
                .lines()
                .find_map(|l| l.strip_prefix("data: "))
                .expect("data line"),
        )
        .expect("valid json");
        assert_eq!(data["type"], "error");
        assert_eq!(data["error"]["type"], "api_error");
        assert_eq!(
            converter.error_message(),
            Some("codex upstream returned no SSE events")
        );
        // The non-streaming aggregate path must refuse to fabricate an
        // empty 200 message out of it.
        assert!(converter.into_message_json().is_none());
    }

    #[test]
    fn done_marker_and_garbage_data_are_ignored() {
        let mut converter = CodexSseConverter::new();
        assert!(converter.on_event("data: [DONE]").is_empty());
        assert!(converter.on_event("data: {not json").is_empty());
        assert!(converter.on_event(": keepalive comment").is_empty());
    }
}
