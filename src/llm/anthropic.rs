//! Anthropic Messages codec (`{base_url}/v1/messages`).
//!
//! Thinking blocks stream as `thinking_delta` text plus a separate
//! `signature_delta`; each block is replayed together as a
//! `{type:"thinking"}` block, keyed by its content-block index so interleaved
//! thinking/text/thinking sequences survive. Redacted thinking keeps only the
//! opaque `data` payload and replays as `{type:"redacted_thinking"}`. A
//! thinking block without a signature is dropped on replay: the strict API
//! rejects unsigned blocks, and converting the chain-of-thought to visible
//! text would leak it into the prompt.

use serde_json::{json, Value};

use super::{
    anthropic_auth, DeltaEvent, LlmError, ModelReply, NeutralMessage, ReasoningBlob, WireRequest,
};
use crate::config::ResolvedProfile;

const DEFAULT_MAX_TOKENS: u64 = 8192;
/// The API requires `max_tokens > thinking.budget_tokens`; this floor keeps
/// room for the visible answer after the thinking budget.
const MIN_ANSWER_TOKENS: u64 = 1024;

pub(crate) fn encode(
    profile: &ResolvedProfile,
    messages: &[NeutralMessage],
    key: Option<&str>,
) -> WireRequest {
    let url = format!("{}/v1/messages", profile.base_url.trim_end_matches('/'));
    let mut system = String::new();
    let mut wire_messages = Vec::new();
    for message in messages {
        match message.role {
            super::Role::System => system.push_str(&message.content),
            super::Role::User => wire_messages.push(json!({
                "role": "user",
                "content": message.content,
            })),
            super::Role::Assistant => {
                let mut blocks = Vec::new();
                if let Some(blob) = &message.reasoning {
                    blocks.extend(replay_blocks(blob, &profile.model));
                }
                if !message.content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": message.content }));
                }
                if !blocks.is_empty() {
                    wire_messages.push(json!({
                        "role": "assistant",
                        "content": blocks,
                    }));
                }
            }
        }
    }
    let mut max_tokens = profile.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut body = json!({
        "model": profile.model,
        "messages": wire_messages,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = Value::String(system);
    }
    if let Some(effort) = &profile.reasoning_effort {
        let budget = budget_tokens(effort);
        max_tokens = max_tokens.max(budget.saturating_add(MIN_ANSWER_TOKENS));
        body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
    body["max_tokens"] = json!(max_tokens);
    WireRequest {
        url,
        auth: anthropic_auth(key),
        body,
    }
}

fn budget_tokens(effort: &str) -> u64 {
    match effort {
        "low" => 2048,
        "medium" => 8192,
        _ => 16384,
    }
}

fn replay_blocks(blob: &ReasoningBlob, model: &str) -> Vec<Value> {
    if !blob.native_to(super::PROTOCOL_ANTHROPIC, model) {
        return Vec::new();
    }
    blob.replay
        .get("blocks")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().filter_map(wire_block).collect())
        .unwrap_or_default()
}

/// Rebuilds one replayed block in wire shape, dropping anything the strict
/// API would reject (unsigned or empty thinking, empty redactions).
fn wire_block(block: &Value) -> Option<Value> {
    match block.get("type").and_then(Value::as_str) {
        Some("thinking") => {
            let signature = block.get("signature").and_then(Value::as_str).unwrap_or("");
            let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
            if signature.trim().is_empty() || text.is_empty() {
                return None;
            }
            Some(json!({ "type": "thinking", "thinking": text, "signature": signature }))
        }
        Some("redacted_thinking") => {
            let data = block.get("data").and_then(Value::as_str).unwrap_or("");
            if data.is_empty() {
                return None;
            }
            Some(json!({ "type": "redacted_thinking", "data": data }))
        }
        _ => None,
    }
}

enum ThinkingBlock {
    Thinking { text: String, signature: String },
    Redacted { data: String },
}

impl ThinkingBlock {
    /// Rebuilds the block in wire shape, dropping anything the strict API
    /// would reject (unsigned or empty thinking, empty redactions).
    fn to_wire(&self) -> Option<Value> {
        match self {
            ThinkingBlock::Thinking { text, signature } => {
                (!text.is_empty() && !signature.trim().is_empty()).then(|| {
                    json!({ "type": "thinking", "thinking": text, "signature": signature })
                })
            }
            ThinkingBlock::Redacted { data } => (!data.is_empty())
                .then(|| json!({ "type": "redacted_thinking", "data": data })),
        }
    }
}

pub(crate) struct Decoder {
    model: String,
    content: String,
    blocks: Vec<(u64, ThinkingBlock)>,
    usage: super::Usage,
    saw_start: bool,
    saw_stop: bool,
    truncated: bool,
}

impl Decoder {
    pub(crate) fn new(model: String) -> Self {
        Self {
            model,
            content: String::new(),
            blocks: Vec::new(),
            usage: super::Usage::default(),
            saw_start: false,
            saw_stop: false,
            truncated: false,
        }
    }

    pub(crate) fn event(
        &mut self,
        name: Option<&str>,
        data: &str,
        on_delta: &mut dyn FnMut(DeltaEvent<'_>),
    ) -> Result<(), LlmError> {
        let Some(name) = name else {
            return Ok(());
        };
        let event: Value = serde_json::from_str(data).map_err(|e| {
            LlmError::protocol(format!("failed to parse stream event {name:?}: {e}"))
        })?;
        match name {
            "message_start" => {
                self.saw_start = true;
                self.usage = parse_usage(event.get("message").and_then(|m| m.get("usage")));
            }
            "content_block_start" => {
                let block_type = event
                    .get("content_block")
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                match block_type {
                    "thinking" => self.blocks.push((
                        index,
                        ThinkingBlock::Thinking {
                            text: String::new(),
                            signature: String::new(),
                        },
                    )),
                    "redacted_thinking" => self.blocks.push((
                        index,
                        ThinkingBlock::Redacted {
                            data: event
                                .get("content_block")
                                .and_then(|block| block.get("data"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        },
                    )),
                    _ => {}
                }
            }
            "content_block_delta" => {
                let delta = event.get("delta");
                let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
                match delta
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        if let Some(text) = delta
                            .and_then(|delta| delta.get("text"))
                            .and_then(Value::as_str)
                        {
                            self.content.push_str(text);
                            on_delta(DeltaEvent::Text(text));
                        }
                    }
                    "thinking_delta" => {
                        if let Some(thinking) = delta
                            .and_then(|delta| delta.get("thinking"))
                            .and_then(Value::as_str)
                        {
                            if let Some((_, ThinkingBlock::Thinking { text, .. })) =
                                self.blocks.iter_mut().find(|(i, _)| *i == index)
                            {
                                text.push_str(thinking);
                            }
                            on_delta(DeltaEvent::Thinking(thinking));
                        }
                    }
                    "signature_delta" => {
                        if let Some(signature) = delta
                            .and_then(|delta| delta.get("signature"))
                            .and_then(Value::as_str)
                        {
                            if let Some((
                                _,
                                ThinkingBlock::Thinking {
                                    signature: slot, ..
                                },
                            )) = self.blocks.iter_mut().find(|(i, _)| *i == index)
                            {
                                slot.push_str(signature);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if event
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str)
                    == Some("max_tokens")
                {
                    self.truncated = true;
                }
                if let Some(usage) = event.get("usage").filter(|usage| !usage.is_null()) {
                    self.usage = merge_usage(self.usage, usage);
                }
            }
            "message_stop" => {
                self.saw_stop = true;
            }
            "error" => {
                return Err(LlmError::protocol(format!(
                    "provider reported error: {}",
                    event
                )));
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<ModelReply, LlmError> {
        if !self.saw_start || !self.saw_stop {
            return Err(LlmError::protocol("stream ended before message_stop"));
        }
        if self.truncated {
            return Err(LlmError::protocol(
                "response truncated by the max_tokens limit before completing; raise max_output_tokens",
            ));
        }
        if self.content.is_empty() {
            return Err(LlmError::protocol("response missing text content"));
        }
        let mut wire_blocks = Vec::new();
        let mut visible: Vec<&str> = Vec::new();
        for (_, block) in &self.blocks {
            if let Some(wire) = block.to_wire() {
                wire_blocks.push(wire);
            }
            if let ThinkingBlock::Thinking { text, .. } = block {
                visible.push(text);
            }
        }
        let reasoning = (!wire_blocks.is_empty()).then(|| ReasoningBlob {
            text: if visible.is_empty() {
                "[reasoning redacted by provider]".to_string()
            } else {
                visible.join("\n\n")
            },
            replay: json!({
                "protocol": super::PROTOCOL_ANTHROPIC,
                "model": self.model,
                "blocks": wire_blocks,
            }),
        });
        Ok(ModelReply {
            content: self.content,
            reasoning,
            usage: self.usage,
        })
    }
}

fn parse_usage(usage: Option<&Value>) -> super::Usage {
    let Some(usage) = usage else {
        return super::Usage::default();
    };
    let number = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
    super::Usage {
        input_tokens: number(usage.get("input_tokens")),
        output_tokens: number(usage.get("output_tokens")),
        cache_read_tokens: number(usage.get("cache_read_input_tokens")),
        cache_write_tokens: number(usage.get("cache_creation_input_tokens")),
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|details| details.get("thinking_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

/// `message_start` seeds the usage snapshot so an early abort still bills the
/// input; `message_delta` overwrites only the fields it actually carries.
fn merge_usage(current: super::Usage, delta: &Value) -> super::Usage {
    let number = |value: Option<&Value>| value.and_then(Value::as_u64);
    let or_keep = |incoming: Option<u64>, kept: u64| incoming.unwrap_or(kept);
    super::Usage {
        input_tokens: or_keep(number(delta.get("input_tokens")), current.input_tokens),
        output_tokens: or_keep(number(delta.get("output_tokens")), current.output_tokens),
        cache_read_tokens: or_keep(
            number(delta.get("cache_read_input_tokens")),
            current.cache_read_tokens,
        ),
        cache_write_tokens: or_keep(
            number(delta.get("cache_creation_input_tokens")),
            current.cache_write_tokens,
        ),
        reasoning_tokens: or_keep(
            delta
                .get("output_tokens_details")
                .and_then(|details| details.get("thinking_tokens"))
                .and_then(Value::as_u64),
            current.reasoning_tokens,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{test_profile, Usage};

    fn profile() -> ResolvedProfile {
        let mut profile = test_profile(
            "anthropic-messages",
            "https://api.deepseek.com/anthropic",
            "deepseek-v4-flash",
        );
        profile.reasoning_effort = Some("medium".into());
        profile
    }

    fn run(events: &[(&str, &str)]) -> Result<ModelReply, LlmError> {
        let mut decoder = Decoder::new("deepseek-v4-flash".into());
        let mut sink: fn(DeltaEvent<'_>) = |_| {};
        for (name, data) in events {
            decoder.event(Some(name), data, &mut sink)?;
        }
        decoder.finish()
    }

    #[test]
    fn decodes_thinking_signature_text_and_usage() {
        let reply = run(&[
            (
                "message_start",
                r#"{"message":{"usage":{"input_tokens":100,"cache_read_input_tokens":60,"cache_creation_input_tokens":10}}}"#,
            ),
            ("content_block_start", r#"{"index":0,"content_block":{"type":"thinking"}}"#),
            ("content_block_delta", r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"ponder "}}"#),
            ("content_block_delta", r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"deeply"}}"#),
            ("content_block_delta", r#"{"index":0,"delta":{"type":"signature_delta","signature":"SIG=="}}"#),
            ("content_block_stop", r#"{}"#),
            ("content_block_start", r#"{"index":1,"content_block":{"type":"text"}}"#),
            ("content_block_delta", r#"{"index":1,"delta":{"type":"text_delta","text":"answer"}}"#),
            (
                "message_delta",
                r#"{"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}}"#,
            ),
            ("message_stop", r#"{}"#),
        ])
        .unwrap();
        assert_eq!(reply.content, "answer");
        let reasoning = reply.reasoning.expect("reasoning captured");
        assert_eq!(reasoning.text, "ponder deeply");
        assert_eq!(
            reasoning.replay,
            json!({
                "protocol": "anthropic-messages",
                "model": "deepseek-v4-flash",
                "blocks": [{ "type": "thinking", "thinking": "ponder deeply", "signature": "SIG==" }]
            })
        );
        assert_eq!(
            reply.usage,
            Usage {
                input_tokens: 100,
                output_tokens: 12,
                cache_read_tokens: 60,
                cache_write_tokens: 10,
                reasoning_tokens: 0,
            }
        );
    }

    /// Thinking, text, then more thinking: each block keeps its own signature
    /// instead of the second block overwriting the first.
    #[test]
    fn interleaved_thinking_blocks_stay_separate() {
        let reply = run(&[
            (
                "message_start",
                r#"{"message":{"usage":{"input_tokens":5}}}"#,
            ),
            (
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"thinking"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"first "}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"signature_delta","signature":"SIG1"}}"#,
            ),
            (
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"text"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"text_delta","text":"mid"}}"#,
            ),
            (
                "content_block_start",
                r#"{"index":2,"content_block":{"type":"thinking"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":2,"delta":{"type":"thinking_delta","thinking":"second"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":2,"delta":{"type":"signature_delta","signature":"SIG2"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"text_delta","text":"end"}}"#,
            ),
            ("message_delta", r#"{"delta":{"stop_reason":"end_turn"}}"#),
            ("message_stop", r#"{}"#),
        ])
        .unwrap();
        assert_eq!(reply.content, "midend");
        let reasoning = reply.reasoning.expect("reasoning captured");
        assert_eq!(reasoning.text, "first \n\nsecond");
        assert_eq!(
            reasoning.replay["blocks"],
            json!([
                { "type": "thinking", "thinking": "first ", "signature": "SIG1" },
                { "type": "thinking", "thinking": "second", "signature": "SIG2" },
            ])
        );
        // The blob replays its blocks in order, then the text block follows.
        let messages = vec![NeutralMessage {
            role: Role::Assistant,
            content: "midend".into(),
            reasoning: Some(reasoning),
        }];
        let wire = encode(&profile(), &messages, None);
        let blocks = &wire.body["messages"][0]["content"];
        assert_eq!(blocks[0]["thinking"], json!("first "));
        assert_eq!(blocks[0]["signature"], json!("SIG1"));
        assert_eq!(blocks[1]["thinking"], json!("second"));
        assert_eq!(blocks[1]["signature"], json!("SIG2"));
        assert_eq!(blocks[2]["type"], json!("text"));
        assert_eq!(blocks[2]["text"], json!("midend"));
    }

    #[test]
    fn redacted_thinking_round_trips_opaquely() {
        let reply = run(&[
            (
                "message_start",
                r#"{"message":{"usage":{"input_tokens":1}}}"#,
            ),
            (
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"redacted_thinking","data":"OPAQUE"}}"#,
            ),
            (
                "content_block_start",
                r#"{"index":1,"content_block":{"type":"text"}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
            ),
            ("message_stop", r#"{}"#),
        ])
        .unwrap();
        let reasoning = reply.reasoning.expect("reasoning captured");
        assert_eq!(reasoning.text, "[reasoning redacted by provider]");
        assert_eq!(
            reasoning.replay["blocks"],
            json!([{ "type": "redacted_thinking", "data": "OPAQUE" }])
        );
        let messages = vec![NeutralMessage {
            role: Role::Assistant,
            content: "ok".into(),
            reasoning: Some(reasoning),
        }];
        let wire = encode(&profile(), &messages, None);
        assert_eq!(
            wire.body["messages"][0]["content"][0],
            json!({"type":"redacted_thinking","data":"OPAQUE"})
        );
    }

    #[test]
    fn stop_reason_max_tokens_is_a_protocol_error() {
        let error = run(&[
            (
                "message_start",
                r#"{"message":{"usage":{"input_tokens":9}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"cut"}}"#,
            ),
            (
                "message_delta",
                r#"{"delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":3}}"#,
            ),
            ("message_stop", r#"{}"#),
        ])
        .unwrap_err();
        assert!(error.message.contains("truncated by the max_tokens limit"));
    }

    #[test]
    fn missing_message_stop_fails() {
        let error = run(&[
            ("message_start", r#"{"message":{"usage":{}}}"#),
            (
                "content_block_delta",
                r#"{"delta":{"type":"text_delta","text":"x"}}"#,
            ),
        ])
        .unwrap_err();
        assert!(error.message.contains("message_stop"));
    }

    #[test]
    fn encode_builds_wire_with_thinking_blocks() {
        let messages = vec![
            NeutralMessage {
                role: Role::System,
                content: "system".into(),
                reasoning: None,
            },
            NeutralMessage {
                role: Role::User,
                content: "hello".into(),
                reasoning: None,
            },
            NeutralMessage {
                role: Role::Assistant,
                content: "hi".into(),
                reasoning: Some(ReasoningBlob {
                    text: String::new(),
                    replay: json!({
                        "protocol": "anthropic-messages",
                        "model": "deepseek-v4-flash",
                        "blocks": [{ "type": "thinking", "thinking": "pondered", "signature": "SIG==" }]
                    }),
                }),
            },
            NeutralMessage {
                role: Role::User,
                content: "next".into(),
                reasoning: None,
            },
            NeutralMessage {
                role: Role::Assistant,
                content: "unstamped".into(),
                reasoning: Some(ReasoningBlob {
                    text: "old shape".into(),
                    replay: json!({"protocol":"anthropic-messages","signature":"OLD=="})
                }),
            },
            NeutralMessage {
                role: Role::Assistant,
                content: "foreign".into(),
                reasoning: Some(ReasoningBlob {
                    text: String::new(),
                    replay: json!({
                        "protocol": "anthropic-messages",
                        "model": "other-model",
                        "blocks": [{ "type": "thinking", "thinking": "swap", "signature": "SIG==" }]
                    }),
                }),
            },
            NeutralMessage {
                role: Role::Assistant,
                content: "unsigned".into(),
                reasoning: Some(ReasoningBlob {
                    text: String::new(),
                    replay: json!({
                        "protocol": "anthropic-messages",
                        "model": "deepseek-v4-flash",
                        "blocks": [{ "type": "thinking", "thinking": "leaky", "signature": "" }]
                    }),
                }),
            },
        ];
        let wire = encode(&profile(), &messages, Some("sk-test"));
        assert_eq!(wire.url, "https://api.deepseek.com/anthropic/v1/messages");
        assert_eq!(wire.body["system"], json!("system"));
        // budget 8192 + the 1024 answer floor; the API requires the headroom.
        assert_eq!(wire.body["max_tokens"], json!(9216));
        assert_eq!(
            wire.body["thinking"],
            json!({"type":"enabled","budget_tokens":8192})
        );
        let assistant = &wire.body["messages"][1];
        assert_eq!(
            assistant["content"][0],
            json!({"type":"thinking","thinking":"pondered","signature":"SIG=="})
        );
        assert_eq!(assistant["content"][1], json!({"type":"text","text":"hi"}));
        // A replay without protocol+model stamps is skipped entirely.
        let unstamped = &wire.body["messages"][3];
        assert_eq!(unstamped["content"].as_array().unwrap().len(), 1);
        assert_eq!(unstamped["content"][0]["type"], json!("text"));
        // A blob stamped for another model is skipped entirely.
        let foreign = &wire.body["messages"][4];
        assert_eq!(foreign["content"].as_array().unwrap().len(), 1);
        assert_eq!(foreign["content"][0]["type"], json!("text"));
        // Unsigned thinking is dropped instead of leaking into visible text.
        let unsigned = &wire.body["messages"][5];
        assert_eq!(unsigned["content"].as_array().unwrap().len(), 1);
        assert_eq!(unsigned["content"][0]["type"], json!("text"));
        match wire.auth.as_ref() {
            Some(WireAuth::Anthropic(key)) => assert_eq!(key, "sk-test"),
            _ => panic!("expected anthropic auth"),
        }
    }

    use crate::llm::{Role, WireAuth};

    #[test]
    fn budget_mapping_matches_pi_levels() {
        assert_eq!(budget_tokens("low"), 2048);
        assert_eq!(budget_tokens("medium"), 8192);
        assert_eq!(budget_tokens("high"), 16384);
    }

    /// An explicit cap below budget + answer floor is raised, not honored.
    #[test]
    fn max_tokens_always_exceeds_the_thinking_budget() {
        let mut tuned = profile();
        tuned.max_output_tokens = Some(8192);
        let messages = vec![NeutralMessage {
            role: Role::User,
            content: "hello".into(),
            reasoning: None,
        }];
        let wire = encode(&tuned, &messages, None);
        assert_eq!(wire.body["max_tokens"], json!(9216));

        tuned.max_output_tokens = Some(16384);
        let wire = encode(&tuned, &messages, None);
        assert_eq!(wire.body["max_tokens"], json!(16384));

        // Without an effort there is no thinking budget to exceed.
        tuned.reasoning_effort = None;
        tuned.max_output_tokens = Some(512);
        let wire = encode(&tuned, &messages, None);
        assert_eq!(wire.body["max_tokens"], json!(512));
        assert!(wire.body.get("thinking").is_none());
    }
}
