//! OpenAI Chat Completions codec (`{base_url}/chat/completions`).
//!
//! Reasoning arrives as `delta.reasoning_content` (DeepSeek) or the
//! `reasoning`/`reasoning_text` variants; whichever field fires first wins and
//! is remembered so the replay puts the text back under the same field.

use serde_json::{json, Value};

use super::{
    bearer_auth, DeltaEvent, LlmError, ModelReply, NeutralMessage, ReasoningBlob, WireRequest,
};
use crate::config::ResolvedProfile;

const REASONING_FIELDS: &[&str] = &["reasoning_content", "reasoning", "reasoning_text"];

pub(crate) fn encode(
    profile: &ResolvedProfile,
    messages: &[NeutralMessage],
    key: Option<&str>,
) -> WireRequest {
    let url = format!(
        "{}/chat/completions",
        profile.base_url.trim_end_matches('/')
    );
    // DeepSeek rejects assistant messages that omit reasoning_content, and
    // its v4 models think by default regardless of reasoning_effort.
    let force_reasoning_field = profile.base_url.to_lowercase().contains("deepseek");
    let mut wire_messages = Vec::with_capacity(messages.len());
    for message in messages {
        let mut wire = json!({ "role": role_name(message.role), "content": message.content });
        if let Some(blob) = &message.reasoning {
            // The remembered field is whatever the capturing stream used, so a
            // provider speaking `reasoning` gets its own field back.
            if let Some(field) = replay_field(blob, &profile.model) {
                if !blob.text.is_empty() {
                    wire[field] = Value::String(blob.text.clone());
                }
            }
        } else if force_reasoning_field && message.role == super::Role::Assistant {
            wire["reasoning_content"] = Value::String(String::new());
        }
        wire_messages.push(wire);
    }
    let mut body = json!({
        "model": profile.model,
        "messages": wire_messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if let Some(value) = profile.max_output_tokens {
        body["max_tokens"] = json!(value);
    }
    if let Some(value) = &profile.reasoning_effort {
        body["reasoning_effort"] = json!(value);
    }
    WireRequest {
        url,
        auth: bearer_auth(key),
        body,
    }
}

fn role_name(role: super::Role) -> &'static str {
    match role {
        super::Role::System => "system",
        super::Role::User => "user",
        super::Role::Assistant => "assistant",
    }
}

fn replay_field(blob: &ReasoningBlob, model: &str) -> Option<&'static str> {
    if !blob.native_to(super::PROTOCOL_CHAT_COMPLETIONS, model) {
        return None;
    }
    let remembered = blob.replay.get("field")?.as_str()?;
    REASONING_FIELDS
        .iter()
        .find(|field| **field == remembered)
        .copied()
}

pub(crate) struct Decoder {
    model: String,
    content: String,
    reasoning_text: String,
    reasoning_field: Option<&'static str>,
    usage: Option<Value>,
    truncated: bool,
}

impl Decoder {
    pub(crate) fn new(model: String) -> Self {
        Self {
            model,
            content: String::new(),
            reasoning_text: String::new(),
            reasoning_field: None,
            usage: None,
            truncated: false,
        }
    }

    pub(crate) fn event(
        &mut self,
        data: &str,
        on_delta: &mut dyn FnMut(DeltaEvent<'_>),
    ) -> Result<(), LlmError> {
        if data.trim() == "[DONE]" {
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data)
            .map_err(|e| LlmError::protocol(format!("failed to parse stream chunk: {e}")))?;
        if let Some(choice) = chunk.get("choices").and_then(|choices| choices.get(0)) {
            if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
                if finish == "length" {
                    self.truncated = true;
                }
            }
            if let Some(delta) = choice.get("delta") {
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        self.content.push_str(text);
                        on_delta(DeltaEvent::Text(text));
                    }
                }
                // First non-empty reasoning field wins; later fields are
                // skipped (not the whole chunk) because some providers mirror
                // the same tokens into both and the usage chunk may follow.
                if let Some((field, text)) = REASONING_FIELDS
                    .iter()
                    .filter_map(|field| {
                        delta
                            .get(*field)
                            .and_then(Value::as_str)
                            .filter(|text| !text.is_empty())
                            .map(|text| (*field, text))
                    })
                    .next()
                {
                    if self.reasoning_field.is_none() {
                        self.reasoning_field = Some(field);
                    }
                    if self.reasoning_field == Some(field) {
                        self.reasoning_text.push_str(text);
                        on_delta(DeltaEvent::Thinking(text));
                    }
                }
            }
        }
        if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null()) {
            self.usage = Some(usage.clone());
        } else if let Some(usage) = chunk
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("usage"))
            .filter(|usage| !usage.is_null())
        {
            // Moonshot-style providers report usage on the choice.
            self.usage = Some(usage.clone());
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<ModelReply, LlmError> {
        if self.truncated {
            return Err(LlmError::protocol(
                "response truncated by the max_tokens limit before completing; raise max_output_tokens",
            ));
        }
        if self.content.is_empty() {
            return Err(LlmError::protocol("response missing text content"));
        }
        let usage = parse_usage(self.usage.as_ref());
        let reasoning = (!self.reasoning_text.is_empty()).then(|| ReasoningBlob {
            text: self.reasoning_text,
            replay: json!({
                "protocol": super::PROTOCOL_CHAT_COMPLETIONS,
                "model": self.model,
                "field": self.reasoning_field.unwrap_or("reasoning_content"),
            }),
        });
        Ok(ModelReply {
            content: self.content,
            reasoning,
            usage,
        })
    }
}

/// Normalizes the provider's usage object. OpenAI reports cached tokens under
/// `prompt_tokens_details.cached_tokens`, DeepSeek under `prompt_cache_hit_tokens`,
/// and Kimi under a top-level `cached_tokens`. `prompt_tokens` already includes
/// the cached share, so `input_tokens` is reported net of cache to match the
/// Anthropic convention (and keep the context-budget arithmetic uniform).
fn parse_usage(usage: Option<&Value>) -> super::Usage {
    let Some(usage) = usage else {
        return super::Usage::default();
    };
    let number = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
    let cache_read_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .or_else(|| usage.get("cached_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write_tokens = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    super::Usage {
        input_tokens: number(usage.get("prompt_tokens"))
            .saturating_sub(cache_read_tokens)
            .saturating_sub(cache_write_tokens),
        output_tokens: number(usage.get("completion_tokens")),
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens: usage
            .get("completion_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{test_profile, Role, Usage, WireAuth};

    fn profile() -> ResolvedProfile {
        let mut profile = test_profile(
            "openai-chat-completions",
            "https://api.deepseek.com",
            "deepseek-v4-flash",
        );
        profile.max_output_tokens = Some(1024);
        profile.reasoning_effort = Some("high".into());
        profile
    }

    fn sink() -> fn(DeltaEvent<'_>) {
        |_| {}
    }

    fn run(chunks: &[&str]) -> Result<ModelReply, LlmError> {
        let mut decoder = Decoder::new("deepseek-v4-flash".into());
        let mut sink = sink();
        for chunk in chunks {
            decoder.event(chunk, &mut sink)?;
        }
        decoder.finish()
    }

    #[test]
    fn decodes_text_reasoning_and_deepseek_usage() {
        let reply = run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"think "}}]}"#,
            r#"{"choices":[{"delta":{"reasoning_content":"hard"}}]}"#,
            r#"{"choices":[{"delta":{"content":"answer"}}]}"#,
            r#"{"choices":[{"delta":{}}],"usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":60,"prompt_cache_miss_tokens":40,"completion_tokens":10,"completion_tokens_details":{"reasoning_tokens":7}}}"#,
            "[DONE]",
        ])
        .unwrap();
        assert_eq!(reply.content, "answer");
        let reasoning = reply.reasoning.expect("reasoning captured");
        assert_eq!(reasoning.text, "think hard");
        assert_eq!(
            reasoning.replay,
            json!({"protocol":"openai-chat-completions","model":"deepseek-v4-flash","field":"reasoning_content"})
        );
        assert_eq!(
            reply.usage,
            Usage {
                input_tokens: 40,
                output_tokens: 10,
                cache_read_tokens: 60,
                cache_write_tokens: 0,
                reasoning_tokens: 7,
            }
        );
    }

    #[test]
    fn first_reasoning_field_wins_and_duplicates_are_ignored() {
        let reply = run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"a"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"MIRRORED"}}]}"#,
            r#"{"choices":[{"delta":{"content":"ok"}}]}"#,
            "[DONE]",
        ])
        .unwrap();
        assert_eq!(reply.reasoning.expect("reasoning captured").text, "a");
    }

    /// The mirrored field arrives in the same chunk as the trailing usage:
    /// skipping the mirror must not skip the usage parse.
    #[test]
    fn mirrored_field_chunk_still_reports_usage() {
        let reply = run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"a"}}]}"#,
            r#"{"choices":[{"delta":{"reasoning":"MIRRORED","content":"ok"}}],"usage":{"prompt_tokens":30,"completion_tokens":2}}"#,
            "[DONE]",
        ])
        .unwrap();
        assert_eq!(reply.content, "ok");
        assert_eq!(reply.reasoning.expect("reasoning captured").text, "a");
        assert_eq!(reply.usage.input_tokens, 30);
        assert_eq!(reply.usage.output_tokens, 2);
    }

    #[test]
    fn finish_reason_length_is_a_protocol_error() {
        let mut decoder = Decoder::new("deepseek-v4-flash".into());
        let mut sink = sink();
        decoder
            .event(
                r#"{"choices":[{"delta":{"content":"cut"},"finish_reason":"length"}],"usage":{"prompt_tokens":10,"completion_tokens":4}}"#,
                &mut sink,
            )
            .unwrap();
        let error = decoder.finish().unwrap_err();
        assert!(error.message.contains("truncated by the max_tokens limit"));
    }

    #[test]
    fn openai_style_usage_is_normalized() {
        let usage = serde_json::json!({
            "prompt_tokens": 50,
            "prompt_tokens_details": { "cached_tokens": 30 },
            "completion_tokens": 5
        });
        assert_eq!(
            parse_usage(Some(&usage)),
            Usage {
                input_tokens: 20,
                output_tokens: 5,
                cache_read_tokens: 30,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
            }
        );
    }

    #[test]
    fn missing_content_is_a_protocol_error() {
        assert!(run(&[
            r#"{"choices":[{"delta":{"reasoning_content":"x"}}]}"#,
            "[DONE]"
        ])
        .is_err());
    }

    /// Chunk shapes captured from the live deepseek-v4-flash stream: a role
    /// first chunk with null content and an empty reasoning string, text
    /// chunks whose reasoning field is null, and the trailing usage chunk.
    #[test]
    fn deepseek_live_stream_shapes_round_trip() {
        let reply = run(&[
            r#"{"id":"d","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":null,"reasoning_content":""},"finish_reason":null}],"usage":null}"#,
            r#"{"id":"d","choices":[{"index":0,"delta":{"content":null,"reasoning_content":"We need"},"finish_reason":null}],"usage":null}"#,
            r#"{"id":"d","choices":[{"index":0,"delta":{"content":"391","reasoning_content":null},"finish_reason":null}],"usage":null}"#,
            r#"{"id":"d","choices":[{"index":0,"delta":{"content":"","reasoning_content":null},"finish_reason":"stop"}],"usage":{"prompt_tokens":2994,"completion_tokens":224,"total_tokens":3218,"prompt_tokens_details":{"cached_tokens":896},"completion_tokens_details":{"reasoning_tokens":197},"prompt_cache_hit_tokens":896,"prompt_cache_miss_tokens":2098}}"#,
            "[DONE]",
        ])
        .unwrap();
        assert_eq!(reply.content, "391");
        assert_eq!(reply.reasoning.expect("reasoning captured").text, "We need");
        assert_eq!(reply.usage.input_tokens, 2098);
        assert_eq!(reply.usage.cache_read_tokens, 896);
        assert_eq!(reply.usage.output_tokens, 224);
        assert_eq!(reply.usage.reasoning_tokens, 197);
    }

    #[test]
    fn encode_replays_reasoning_and_forces_the_field_for_deepseek() {
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
                    text: "pondered".into(),
                    replay: json!({"protocol":"openai-chat-completions","model":"deepseek-v4-flash","field":"reasoning_content"}),
                }),
            },
            NeutralMessage {
                role: Role::User,
                content: "next".into(),
                reasoning: None,
            },
            // DeepSeek thinks by default, so every assistant message carries
            // the reasoning_content field even when replay text is absent.
            NeutralMessage {
                role: Role::Assistant,
                content: "fresh".into(),
                reasoning: None,
            },
        ];
        let wire = encode(&profile(), &messages, Some("sk-test"));
        assert_eq!(wire.url, "https://api.deepseek.com/chat/completions");
        assert_eq!(wire.body["stream"], json!(true));
        assert_eq!(wire.body["stream_options"]["include_usage"], json!(true));
        assert_eq!(wire.body["max_tokens"], json!(1024));
        assert_eq!(wire.body["reasoning_effort"], json!("high"));
        let replayed = &wire.body["messages"][2];
        assert_eq!(replayed["role"], json!("assistant"));
        assert_eq!(replayed["content"], json!("hi"));
        assert_eq!(replayed["reasoning_content"], json!("pondered"));
        let forced = &wire.body["messages"][4];
        assert_eq!(forced["reasoning_content"], json!(""));
        match wire.auth.as_ref() {
            Some(WireAuth::Bearer(key)) => assert_eq!(key, "sk-test"),
            _ => panic!("expected bearer auth"),
        }
    }

    /// A replay blob stamped for another model — or missing its stamps — is
    /// skipped rather than replayed under the wrong assumptions.
    #[test]
    fn foreign_replay_shapes_are_skipped() {
        let unstamped = vec![NeutralMessage {
            role: Role::Assistant,
            content: "legacy".into(),
            reasoning: Some(ReasoningBlob {
                text: "pondered".into(),
                replay: json!({"protocol":"openai-chat-completions","field":"reasoning_content"}),
            }),
        }];
        let wire = encode(&profile(), &unstamped, None);
        assert!(wire.body["messages"][0].get("reasoning_content").is_none());

        let foreign_model = vec![NeutralMessage {
            role: Role::Assistant,
            content: "swap".into(),
            reasoning: Some(ReasoningBlob {
                text: "pondered".into(),
                replay: json!({"protocol":"openai-chat-completions","model":"other-model","field":"reasoning_content"}),
            }),
        }];
        let wire = encode(&profile(), &foreign_model, None);
        assert!(wire.body["messages"][0].get("reasoning_content").is_none());

        let foreign_protocol = vec![NeutralMessage {
            role: Role::Assistant,
            content: "hi".into(),
            reasoning: Some(ReasoningBlob {
                text: "pondered".into(),
                replay: json!({"protocol":"anthropic-messages","signature":"sig"}),
            }),
        }];
        let wire = encode(&profile(), &foreign_protocol, None);
        assert!(wire.body["messages"][0].get("reasoning_content").is_none());
    }

    /// A provider that speaks `reasoning` instead of `reasoning_content` gets
    /// its own field back on replay.
    #[test]
    fn replay_uses_whichever_field_the_stream_used() {
        let messages = vec![NeutralMessage {
            role: Role::Assistant,
            content: "hi".into(),
            reasoning: Some(ReasoningBlob {
                text: "pondered".into(),
                replay: json!({"protocol":"openai-chat-completions","model":"deepseek-v4-flash","field":"reasoning"}),
            }),
        }];
        let wire = encode(&profile(), &messages, None);
        assert_eq!(wire.body["messages"][0]["reasoning"], json!("pondered"));
        assert!(wire.body["messages"][0].get("reasoning_content").is_none());
    }

    /// An empty credential sends no Authorization header at all.
    #[test]
    fn empty_key_omits_the_auth_header() {
        let messages = vec![NeutralMessage {
            role: Role::User,
            content: "hello".into(),
            reasoning: None,
        }];
        let wire = encode(&profile(), &messages, Some(""));
        assert!(wire.auth.is_none());
    }
}
