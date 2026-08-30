//! OpenAI Responses codec (`{base_url}/responses`).
//!
//! Reasoning is replayed as the provider's own reasoning items: each
//! `response.output_item.done` reasoning item (which carries
//! `encrypted_content` when requested) is stored verbatim and pushed back into
//! the next request's input array. The conversation stays stateless
//! (`store: false`) because the encrypted reasoning blob is the only replay
//! currency.

use serde_json::{json, Value};

use super::{
    bearer_auth, DeltaEvent, LlmError, ModelReply, NeutralMessage, ReasoningBlob, WireRequest,
};
use crate::config::ResolvedProfile;

pub(crate) fn encode(
    profile: &ResolvedProfile,
    messages: &[NeutralMessage],
    key: Option<&str>,
) -> WireRequest {
    let url = format!("{}/responses", profile.base_url.trim_end_matches('/'));
    let mut input = Vec::new();
    for message in messages {
        match message.role {
            super::Role::System => input.push(json!({
                "role": "system",
                "content": message.content,
            })),
            super::Role::User => input.push(json!({
                "role": "user",
                "content": [{ "type": "input_text", "text": message.content }],
            })),
            super::Role::Assistant => {
                if let Some(blob) = &message.reasoning {
                    for item in replay_items(blob, &profile.model) {
                        input.push(item.clone());
                    }
                }
                if !message.content.is_empty() {
                    input.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": message.content,
                            "annotations": [],
                        }],
                        "status": "completed",
                    }));
                }
            }
        }
    }
    let mut body = json!({
        "model": profile.model,
        "input": input,
        "stream": true,
        "store": false,
    });
    if let Some(value) = profile.max_output_tokens {
        // The API rejects tiny caps outright; anything sane passes through.
        body["max_output_tokens"] = json!(value.max(16));
    }
    if let Some(effort) = &profile.reasoning_effort {
        body["reasoning"] = json!({ "effort": effort, "summary": "auto" });
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    WireRequest {
        url,
        auth: bearer_auth(key),
        body,
    }
}

fn replay_items<'a>(blob: &'a ReasoningBlob, model: &str) -> &'a [Value] {
    if !blob.native_to(super::PROTOCOL_RESPONSES, model) {
        return &[];
    }
    blob.replay
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

pub(crate) struct Decoder {
    model: String,
    content: String,
    reasoning_text: String,
    reasoning_items: Vec<Value>,
    usage: Option<Value>,
    saw_terminal: bool,
}

impl Decoder {
    pub(crate) fn new(model: String) -> Self {
        Self {
            model,
            content: String::new(),
            reasoning_text: String::new(),
            reasoning_items: Vec::new(),
            usage: None,
            saw_terminal: false,
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
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.reasoning_text.push_str(delta);
                    on_delta(DeltaEvent::Thinking(delta));
                }
            }
            "response.reasoning_summary_part.done" => {
                self.reasoning_text.push_str("\n\n");
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                    self.content.push_str(delta);
                    on_delta(DeltaEvent::Text(delta));
                }
            }
            "response.output_item.done" => {
                if event
                    .get("item")
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("reasoning")
                {
                    if let Some(item) = event.get("item") {
                        self.merge_reasoning_item(item);
                    }
                }
            }
            "response.completed" | "response.incomplete" => {
                self.saw_terminal = true;
                if let Some(response) = event.get("response") {
                    if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
                        self.usage = Some(usage.clone());
                    }
                    // The terminal event's snapshot is authoritative: some
                    // providers omit encrypted_content from the incremental
                    // output_item.done but carry it here.
                    if let Some(items) = response.get("output").and_then(Value::as_array) {
                        for item in items {
                            if item.get("type").and_then(Value::as_str) == Some("reasoning") {
                                self.merge_reasoning_item(item);
                            }
                        }
                    }
                }
                if name == "response.incomplete" {
                    let reason = event
                        .get("response")
                        .and_then(|response| response.get("incomplete_details"))
                        .and_then(|details| details.get("reason"))
                        .cloned()
                        .unwrap_or(Value::Null);
                    return Err(LlmError::protocol(format!(
                        "response ended incomplete ({reason}); raise max_output_tokens if the reason is max_output_tokens"
                    )));
                }
            }
            "response.failed" => {
                let error = event
                    .get("response")
                    .and_then(|response| response.get("error"));
                return Err(LlmError::protocol(format!(
                    "provider reported response.failed: {}",
                    error.unwrap_or(&Value::Null)
                )));
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

    /// Adds a reasoning item, replacing an earlier capture with the same id so
    /// the terminal snapshot can backfill fields the incremental event lacked.
    fn merge_reasoning_item(&mut self, item: &Value) {
        if let Some(id) = item.get("id").and_then(Value::as_str) {
            if let Some(slot) = self
                .reasoning_items
                .iter_mut()
                .find(|existing| existing.get("id").and_then(Value::as_str) == Some(id))
            {
                *slot = item.clone();
                return;
            }
        }
        self.reasoning_items.push(item.clone());
    }

    pub(crate) fn finish(self) -> Result<ModelReply, LlmError> {
        if !self.saw_terminal {
            return Err(LlmError::protocol(
                "stream ended before a terminal response event",
            ));
        }
        if self.content.is_empty() {
            return Err(LlmError::protocol("response missing text content"));
        }
        let usage = parse_usage(self.usage.as_ref());
        let reasoning =
            (!self.reasoning_text.is_empty() || !self.reasoning_items.is_empty()).then(|| {
                ReasoningBlob {
                    text: self.reasoning_text,
                    replay: json!({
                        "protocol": super::PROTOCOL_RESPONSES,
                        "model": self.model,
                        "items": self.reasoning_items,
                    }),
                }
            });
        Ok(ModelReply {
            content: self.content,
            reasoning,
            usage,
        })
    }
}

/// `input_tokens` already includes the cached and cache-write shares, so both
/// are subtracted to report net input (matching the Anthropic convention and
/// keeping the context-budget arithmetic uniform).
fn parse_usage(usage: Option<&Value>) -> super::Usage {
    let Some(usage) = usage else {
        return super::Usage::default();
    };
    let number = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
    let cache_read_tokens = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write_tokens = usage
        .get("input_tokens_details")
        .and_then(|details| details.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    super::Usage {
        input_tokens: number(usage.get("input_tokens"))
            .saturating_sub(cache_read_tokens)
            .saturating_sub(cache_write_tokens),
        output_tokens: number(usage.get("output_tokens")),
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens: usage
            .get("output_tokens_details")
            .and_then(|details| details.get("reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{test_profile, Role, Usage};

    fn profile() -> ResolvedProfile {
        let mut profile = test_profile("openai-responses", "https://api.openai.com/v1", "gpt-test");
        profile.max_output_tokens = Some(2048);
        profile.reasoning_effort = Some("medium".into());
        profile
    }

    fn run(events: &[(&str, &str)]) -> Result<ModelReply, LlmError> {
        let mut decoder = Decoder::new("gpt-test".into());
        let mut sink: fn(DeltaEvent<'_>) = |_| {};
        for (name, data) in events {
            decoder.event(Some(name), data, &mut sink)?;
        }
        decoder.finish()
    }

    #[test]
    fn decodes_reasoning_and_text_stream() {
        let reply = run(&[
            (
                "response.output_item.added",
                r#"{"output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
            ),
            (
                "response.reasoning_text.delta",
                r#"{"delta":"ponder "}"#,
            ),
            (
                "response.reasoning_text.delta",
                r#"{"delta":"deeply"}"#,
            ),
            (
                "response.output_item.done",
                r#"{"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"ENC"}}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"delta":"answer"}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"usage":{"input_tokens":90,"input_tokens_details":{"cached_tokens":40},"output_tokens":12,"output_tokens_details":{"reasoning_tokens":8},"total_tokens":102}}}"#,
            ),
        ])
        .unwrap();
        assert_eq!(reply.content, "answer");
        let reasoning = reply.reasoning.expect("reasoning captured");
        assert_eq!(reasoning.text, "ponder deeply");
        assert_eq!(
            reasoning.replay,
            json!({
                "protocol": "openai-responses",
                "model": "gpt-test",
                "items": [{ "type": "reasoning", "id": "rs_1", "summary": [], "encrypted_content": "ENC" }]
            })
        );
        assert_eq!(
            reply.usage,
            Usage {
                input_tokens: 50,
                output_tokens: 12,
                cache_read_tokens: 40,
                cache_write_tokens: 0,
                reasoning_tokens: 8,
            }
        );
    }

    /// Azure-style streams omit encrypted_content from output_item.done; the
    /// terminal snapshot must backfill it by id instead of duplicating the item.
    #[test]
    fn terminal_event_backfills_encrypted_content() {
        let reply = run(&[
            (
                "response.output_item.done",
                r#"{"output_index":0,"item":{"type":"reasoning","id":"rs_1","summary":[]}}"#,
            ),
            (
                "response.output_text.delta",
                r#"{"delta":"answer"}"#,
            ),
            (
                "response.completed",
                r#"{"response":{"output":[{"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"BACKFILLED"}],"usage":{"input_tokens":10,"output_tokens":2}}}"#,
            ),
        ])
        .unwrap();
        let items = reply.reasoning.expect("reasoning captured").replay["items"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["encrypted_content"], json!("BACKFILLED"));
    }

    #[test]
    fn incomplete_response_is_a_protocol_error() {
        let error = run(&[
            (
                "response.output_text.delta",
                r#"{"delta":"cut"}"#,
            ),
            (
                "response.incomplete",
                r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":10,"output_tokens":2}}}"#,
            ),
        ])
        .unwrap_err();
        assert!(error.message.contains("incomplete"));
        assert!(error.message.contains("max_output_tokens"));
    }

    #[test]
    fn stream_without_terminal_event_fails() {
        let error = run(&[("response.output_text.delta", r#"{"delta":"x"}"#)]).unwrap_err();
        assert!(error.message.contains("terminal"));
    }

    #[test]
    fn encode_replays_reasoning_items_statelessly() {
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
                        "protocol": "openai-responses",
                        "model": "gpt-test",
                        "items": [{ "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC" }]
                    }),
                }),
            },
        ];
        let wire = encode(&profile(), &messages, Some("sk-test"));
        assert_eq!(wire.url, "https://api.openai.com/v1/responses");
        assert_eq!(wire.body["store"], json!(false));
        assert_eq!(wire.body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(
            wire.body["reasoning"],
            json!({"effort":"medium","summary":"auto"})
        );
        let input = wire.body["input"].as_array().unwrap();
        assert_eq!(input[0], json!({"role":"system","content":"system"}));
        assert_eq!(
            input[1],
            json!({"role":"user","content":[{"type":"input_text","text":"hello"}]})
        );
        assert_eq!(input[2]["type"], json!("reasoning"));
        assert_eq!(input[2]["encrypted_content"], json!("ENC"));
        assert_eq!(input[3]["type"], json!("message"));
        assert_eq!(input[3]["role"], json!("assistant"));
        assert_eq!(input[3]["content"][0]["type"], json!("output_text"));
        assert_eq!(input[3]["content"][0]["text"], json!("hi"));
    }

    #[test]
    fn foreign_replay_shapes_are_skipped() {
        let foreign_model = vec![NeutralMessage {
            role: Role::Assistant,
            content: "swap".into(),
            reasoning: Some(ReasoningBlob {
                text: String::new(),
                replay: json!({
                    "protocol": "openai-responses",
                    "model": "other-model",
                    "items": [{ "type": "reasoning", "id": "rs_1", "encrypted_content": "ENC" }]
                }),
            }),
        }];
        let wire = encode(&profile(), &foreign_model, None);
        let input = wire.body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], json!("message"));

        let foreign_protocol = vec![NeutralMessage {
            role: Role::Assistant,
            content: "hi".into(),
            reasoning: Some(ReasoningBlob {
                text: "pondered".into(),
                replay: json!({"protocol":"openai-chat-completions","field":"reasoning_content"}),
            }),
        }];
        let wire = encode(&profile(), &foreign_protocol, None);
        let input = wire.body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], json!("message"));
    }

    #[test]
    fn tiny_output_caps_are_floored() {
        let mut tuned = profile();
        tuned.max_output_tokens = Some(4);
        let messages = vec![NeutralMessage {
            role: Role::User,
            content: "hello".into(),
            reasoning: None,
        }];
        let wire = encode(&tuned, &messages, None);
        assert_eq!(wire.body["max_output_tokens"], json!(16));
    }
}
