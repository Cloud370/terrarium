//! Streaming LLM transport.
//!
//! The transport owns the HTTP client, timeout policy, and SSE plumbing;
//! per-protocol codecs own request encoding and stream decoding for the three
//! supported wire protocols: `openai-chat-completions`, `openai-responses`,
//! and `anthropic-messages`. Assistant reasoning (chain-of-thought, provider
//! signatures, encrypted reasoning items) round-trips through the session so
//! the next request replays it in whatever shape the protocol requires.

mod anthropic;
mod chat_completions;
mod responses;
mod sse;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Serialize;

use crate::config::ResolvedProfile;

pub(crate) const RESPONSE_BODY_CAP: usize = 4 * 1024 * 1024;
pub(crate) const ERROR_BODY_CAP: usize = 8 * 1024;
pub(crate) const REASONING_TEXT_CAP: usize = 256 * 1024;
pub(crate) const REASONING_REPLAY_CAP: usize = 1024 * 1024;

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 300_000;
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 120_000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub const PROTOCOL_CHAT_COMPLETIONS: &str = "openai-chat-completions";
pub const PROTOCOL_RESPONSES: &str = "openai-responses";
pub const PROTOCOL_ANTHROPIC: &str = "anthropic-messages";

// ---------------------------------------------------------------------------
// Neutral message model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Provider-opaque reasoning replay data. Each codec writes a replay shape it
/// recognizes (carrying `protocol` and `model` markers); a mismatched shape is
/// skipped so resuming a session under a different protocol or model never
/// corrupts the request with foreign reasoning payloads.
#[derive(Debug, Clone, PartialEq)]
pub struct ReasoningBlob {
    pub text: String,
    pub replay: serde_json::Value,
}

impl ReasoningBlob {
    /// True when this blob was captured by the same protocol and model and
    /// may be replayed verbatim.
    pub(crate) fn native_to(&self, protocol: &str, model: &str) -> bool {
        let stamp = |key: &str| self.replay.get(key).and_then(serde_json::Value::as_str);
        stamp("protocol") == Some(protocol) && stamp("model") == Some(model)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NeutralMessage {
    pub role: Role,
    pub content: String,
    pub reasoning: Option<ReasoningBlob>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    /// Approximation of the context the *next* request will carry: everything
    /// the provider billed for this exchange, cached or not.
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.output_tokens)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelReply {
    pub content: String,
    pub reasoning: Option<ReasoningBlob>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaEvent<'a> {
    Thinking(&'a str),
    Text(&'a str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmError {
    pub kind: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl LlmError {
    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self {
            kind: "protocol",
            message: message.into(),
            retryable: false,
        }
    }

    pub(crate) fn transport(message: impl Into<String>) -> Self {
        Self {
            kind: "transport",
            message: message.into(),
            retryable: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Process-wide accounting
// ---------------------------------------------------------------------------

static LLM_CALLS: AtomicU64 = AtomicU64::new(0);
static LLM_INPUT_TOKENS: AtomicU64 = AtomicU64::new(0);
static LLM_OUTPUT_TOKENS: AtomicU64 = AtomicU64::new(0);
static LLM_CACHE_READ_TOKENS: AtomicU64 = AtomicU64::new(0);
static LLM_CACHE_WRITE_TOKENS: AtomicU64 = AtomicU64::new(0);
static LLM_REASONING_TOKENS: AtomicU64 = AtomicU64::new(0);

pub fn usage_json() -> serde_json::Value {
    serde_json::json!({
        "calls": LLM_CALLS.load(Ordering::Relaxed),
        "input_tokens": LLM_INPUT_TOKENS.load(Ordering::Relaxed),
        "output_tokens": LLM_OUTPUT_TOKENS.load(Ordering::Relaxed),
        "cache_read_tokens": LLM_CACHE_READ_TOKENS.load(Ordering::Relaxed),
        "cache_write_tokens": LLM_CACHE_WRITE_TOKENS.load(Ordering::Relaxed),
        "reasoning_tokens": LLM_REASONING_TOKENS.load(Ordering::Relaxed),
    })
}

fn account_usage(usage: &Usage) {
    LLM_INPUT_TOKENS.fetch_add(usage.input_tokens, Ordering::Relaxed);
    LLM_OUTPUT_TOKENS.fetch_add(usage.output_tokens, Ordering::Relaxed);
    LLM_CACHE_READ_TOKENS.fetch_add(usage.cache_read_tokens, Ordering::Relaxed);
    LLM_CACHE_WRITE_TOKENS.fetch_add(usage.cache_write_tokens, Ordering::Relaxed);
    LLM_REASONING_TOKENS.fetch_add(usage.reasoning_tokens, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

pub(crate) fn http_client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .expect("http client")
    })
}

pub(crate) struct WireRequest {
    pub url: String,
    pub auth: Option<WireAuth>,
    pub body: serde_json::Value,
}

pub(crate) enum WireAuth {
    Bearer(String),
    Anthropic(String),
}

/// An absent or empty credential sends no auth header at all — local and
/// development endpoints would otherwise reject a blank `Authorization` value.
pub(crate) fn bearer_auth(key: Option<&str>) -> Option<WireAuth> {
    non_empty(key).map(|key| WireAuth::Bearer(key.to_string()))
}

pub(crate) fn anthropic_auth(key: Option<&str>) -> Option<WireAuth> {
    non_empty(key).map(|key| WireAuth::Anthropic(key.to_string()))
}

fn non_empty(key: Option<&str>) -> Option<&str> {
    key.filter(|key| !key.is_empty())
}

pub(crate) enum Codec {
    ChatCompletions,
    Responses,
    Anthropic,
}

fn codec_for(protocol: &str) -> Result<Codec, LlmError> {
    match protocol {
        PROTOCOL_CHAT_COMPLETIONS => Ok(Codec::ChatCompletions),
        PROTOCOL_RESPONSES => Ok(Codec::Responses),
        PROTOCOL_ANTHROPIC => Ok(Codec::Anthropic),
        other => Err(LlmError {
            kind: "configuration",
            message: format!("unsupported protocol {other:?}"),
            retryable: false,
        }),
    }
}

/// Whether `protocol` names a built-in wire protocol. Configuration and
/// journal validation share this with codec selection so the sets cannot drift.
pub fn protocol_supported(protocol: &str) -> bool {
    codec_for(protocol).is_ok()
}

/// Stream one completion attempt. `on_delta` receives thinking/text deltas as
/// they arrive (the operator's live preview; pass a no-op to silence it).
pub async fn stream(
    profile: &ResolvedProfile,
    messages: Vec<NeutralMessage>,
    on_delta: &mut dyn FnMut(DeltaEvent<'_>),
) -> Result<ModelReply, LlmError> {
    let request_timeout = Duration::from_millis(
        profile
            .request_timeout_ms
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
    );
    let idle_timeout =
        Duration::from_millis(profile.idle_timeout_ms.unwrap_or(DEFAULT_IDLE_TIMEOUT_MS));
    match tokio::time::timeout(
        request_timeout,
        stream_inner(profile, messages, idle_timeout, on_delta),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(LlmError {
            kind: "timeout",
            message: format!(
                "request exceeded the {}ms total budget",
                request_timeout.as_millis()
            ),
            retryable: true,
        }),
    }
}

async fn stream_inner(
    profile: &ResolvedProfile,
    messages: Vec<NeutralMessage>,
    idle_timeout: Duration,
    on_delta: &mut dyn FnMut(DeltaEvent<'_>),
) -> Result<ModelReply, LlmError> {
    let key = profile
        .api_key_env
        .as_ref()
        .map(|name| std::env::var(name).unwrap_or_default());
    if profile.api_key_env.is_some() && key.as_deref().unwrap_or_default().is_empty() {
        return Err(LlmError {
            kind: "configuration",
            message: format!(
                "credential environment variable {:?} is not set",
                profile.api_key_env.as_deref().unwrap_or_default()
            ),
            retryable: false,
        });
    }
    let codec = codec_for(&profile.protocol)?;
    let debug = std::env::var_os("TERRARIUM_LLM_DEBUG").is_some_and(|value| value != "0");
    let wire = match &codec {
        Codec::ChatCompletions => chat_completions::encode(profile, &messages, key.as_deref()),
        Codec::Responses => responses::encode(profile, &messages, key.as_deref()),
        Codec::Anthropic => anthropic::encode(profile, &messages, key.as_deref()),
    };
    LLM_CALLS.fetch_add(1, Ordering::Relaxed);
    if debug {
        eprintln!("terrarium llm → POST {}", wire.url);
        eprintln!(
            "terrarium llm → {}",
            serde_json::to_string_pretty(&wire.body).unwrap_or_default()
        );
    }
    let mut request = http_client().post(&wire.url).json(&wire.body);
    match wire.auth.as_ref() {
        Some(WireAuth::Bearer(key)) => request = request.bearer_auth(key),
        Some(WireAuth::Anthropic(key)) => {
            request = request
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01");
        }
        None => {}
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| LlmError::transport(format!("request failed: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = read_error_body(&mut response).await;
        let message = match body {
            Some(snippet) => format!("HTTP {status}: {snippet}"),
            None => format!("HTTP {status}"),
        };
        return Err(LlmError {
            kind: "http",
            message,
            retryable: status.as_u16() == 429 || status.is_server_error(),
        });
    }
    let mut decoder = sse::SseDecoder::new();
    let mut reply = match codec {
        Codec::ChatCompletions => {
            ReplyDecoder::Chat(chat_completions::Decoder::new(profile.model.clone()))
        }
        Codec::Responses => ReplyDecoder::Responses(responses::Decoder::new(profile.model.clone())),
        Codec::Anthropic => ReplyDecoder::Anthropic(anthropic::Decoder::new(profile.model.clone())),
    };
    let mut received = 0usize;
    let mut events = Vec::new();
    loop {
        let chunk = tokio::time::timeout(idle_timeout, response.chunk())
            .await
            .map_err(|_| LlmError {
                kind: "timeout",
                message: format!(
                    "stream stalled for over {}ms without a chunk",
                    idle_timeout.as_millis()
                ),
                retryable: true,
            })?
            .map_err(|e| LlmError::transport(format!("failed to read stream: {e}")))?;
        let Some(chunk) = chunk else { break };
        received += chunk.len();
        if received > RESPONSE_BODY_CAP {
            return Err(LlmError {
                kind: "http",
                message: format!("provider stream exceeds the {RESPONSE_BODY_CAP}-byte limit"),
                retryable: false,
            });
        }
        events.clear();
        decoder.feed(&chunk, &mut events);
        for event in events.drain(..) {
            if debug {
                eprintln!(
                    "terrarium llm ← event: {:?} data: {}",
                    event.event, event.data
                );
            }
            reply.event(event.event.as_deref(), &event.data, on_delta)?;
        }
    }
    events.clear();
    decoder.finish(&mut events);
    for event in events.drain(..) {
        reply.event(event.event.as_deref(), &event.data, on_delta)?;
    }
    let reply = reply.finish()?;
    account_usage(&reply.usage);
    Ok(reply)
}

enum ReplyDecoder {
    Chat(chat_completions::Decoder),
    Responses(responses::Decoder),
    Anthropic(anthropic::Decoder),
}

impl ReplyDecoder {
    fn event(
        &mut self,
        name: Option<&str>,
        data: &str,
        on_delta: &mut dyn FnMut(DeltaEvent<'_>),
    ) -> Result<(), LlmError> {
        match self {
            ReplyDecoder::Chat(decoder) => decoder.event(data, on_delta),
            ReplyDecoder::Responses(decoder) => decoder.event(name, data, on_delta),
            ReplyDecoder::Anthropic(decoder) => decoder.event(name, data, on_delta),
        }
    }

    fn finish(self) -> Result<ModelReply, LlmError> {
        match self {
            ReplyDecoder::Chat(decoder) => decoder.finish(),
            ReplyDecoder::Responses(decoder) => decoder.finish(),
            ReplyDecoder::Anthropic(decoder) => decoder.finish(),
        }
    }
}

async fn read_error_body(response: &mut reqwest::Response) -> Option<String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok().flatten() {
        body.extend_from_slice(&chunk);
        if body.len() >= ERROR_BODY_CAP {
            body.truncate(ERROR_BODY_CAP);
            break;
        }
    }
    if body.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&body).trim().to_string())
}

/// One shared constructor so every test profile stays in sync when
/// `ResolvedProfile` grows a field.
#[cfg(test)]
pub(crate) fn test_profile(protocol: &str, base_url: &str, model: &str) -> ResolvedProfile {
    ResolvedProfile {
        name: "test".into(),
        protocol: protocol.into(),
        base_url: base_url.into(),
        api_key_env: None,
        model: model.into(),
        max_output_tokens: None,
        reasoning_effort: None,
        request_timeout_ms: None,
        idle_timeout_ms: None,
        context_window: None,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn protocol_constants_cover_the_three_wire_shapes() {
        assert_eq!(super::PROTOCOL_CHAT_COMPLETIONS, "openai-chat-completions");
        assert_eq!(super::PROTOCOL_RESPONSES, "openai-responses");
        assert_eq!(super::PROTOCOL_ANTHROPIC, "anthropic-messages");
    }
}
