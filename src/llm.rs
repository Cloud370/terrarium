//! host.llm —— nested model calls: call (one-shot) / chat (multi-turn, auto-prepends contract).
//! Async-concurrent (Promise.all = max not sum), cancellable (watch token), transport hangs self-heal via 120s × 2 retries.
//! Keys live only in this process's env; the sandbox can't see them. (usage/contract introspection cut in D017 —
//! Rust-side usage_snapshot below still feeds the operator's stderr stats.)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use rquickjs::function::{Async, Opt};
use rquickjs::{Ctx, Function, Object};
use serde::Serialize;
use tokio::sync::watch;

const RESPONSE_BODY_CAP: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelCapabilities {
    pub id: &'static str,
    pub input_modalities: &'static [Modality],
    pub output_modalities: &'static [Modality],
}

const TEXT_INPUT: &[Modality] = &[Modality::Text];
const TEXT_OUTPUT: &[Modality] = &[Modality::Text];
const TEXT_IMAGE_INPUT: &[Modality] = &[Modality::Text, Modality::Image];

const MODEL_CAPABILITIES: &[ModelCapabilities] = &[
    ModelCapabilities {
        id: "deepseek-v4-flash",
        input_modalities: TEXT_INPUT,
        output_modalities: TEXT_OUTPUT,
    },
    ModelCapabilities {
        id: "deepseek-v4-flash-vision-exp",
        input_modalities: TEXT_IMAGE_INPUT,
        output_modalities: TEXT_OUTPUT,
    },
];

pub fn model_capabilities() -> &'static [ModelCapabilities] {
    MODEL_CAPABILITIES
}

pub(crate) fn capability_text(model: &str) -> String {
    let Some(capability) = model_capabilities().iter().find(|item| item.id == model) else {
        return format!(
            "Configured LLM model: {model} (capabilities are not declared locally; requests remain text-only)"
        );
    };
    let inputs = capability
        .input_modalities
        .iter()
        .map(|modality| match modality {
            Modality::Text => "text",
            Modality::Image => "image",
        })
        .collect::<Vec<_>>()
        .join(", ");
    let outputs = capability
        .output_modalities
        .iter()
        .map(|modality| match modality {
            Modality::Text => "text",
            Modality::Image => "image",
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Configured LLM model: {model} (declared input: {inputs}; declared output: {outputs}; current host.llm payloads are text-only)"
    )
}

static LLM_CALLS: AtomicU64 = AtomicU64::new(0);
static LLM_CACHE_HIT: AtomicU64 = AtomicU64::new(0);
static LLM_CACHE_MISS: AtomicU64 = AtomicU64::new(0);
static LLM_OUT_TOKENS: AtomicU64 = AtomicU64::new(0);
/// Concurrency bound for nested LLM calls — the network's analogue of the cage's heap limit.
/// A Promise.all fanout beyond this queues instead of exhausting fds at the provider.
static LLM_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(32);

#[derive(Clone)]
struct ChatMsg {
    role: String,
    content: String,
}

impl<'js> rquickjs::FromJs<'js> for ChatMsg {
    fn from_js(_ctx: &rquickjs::Ctx<'js>, value: rquickjs::Value<'js>) -> rquickjs::Result<Self> {
        let obj = value.as_object().ok_or_else(|| rquickjs::Error::FromJs {
            from: "message",
            to: "object",
            message: Some("message must be {role, content}".into()),
        })?;
        let role: String = obj.get("role")?;
        // shape validation, not judgment: the system prompt has its own argument, and accepting
        // an injected {role:"system"} would let mounted data talk past the contract
        if role != "user" && role != "assistant" {
            return Err(rquickjs::Error::FromJs {
                from: "message",
                to: "message",
                message: Some(format!(
                    "message.role must be 'user' or 'assistant' (got {role:?}); \
                     the system prompt is the `system` argument"
                )),
            });
        }
        Ok(ChatMsg {
            role,
            content: obj.get("content")?,
        })
    }
}

fn http_client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(120)) // per-request cap; transport-layer hangs self-heal via retry
            .build()
            .expect("http client")
    })
}

fn base_url() -> String {
    std::env::var("TERRARIUM_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com/chat/completions".into())
}

/// Model identity used by the outer agent and nested calls.
pub fn model_name() -> String {
    std::env::var("TERRARIUM_LLM_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".into())
}

/// Rust-side entry for the outer agent loop: same transport/retry/accounting path as host.llm.
pub async fn complete(messages: Vec<serde_json::Value>) -> Result<String, String> {
    do_llm(messages).await.map_err(|e| match e {
        // strip the rquickjs "converting from js … into type host.llm" shell — meaningless to a Rust caller
        rquickjs::Error::FromJs {
            message: Some(m), ..
        } => m,
        other => other.to_string(),
    })
}

fn llm_err(from: &'static str, msg: String) -> rquickjs::Error {
    rquickjs::Error::FromJs {
        from,
        to: "host.llm",
        message: Some(msg),
    }
}

async fn read_json_response(
    mut response: reqwest::Response,
) -> Result<serde_json::Value, rquickjs::Error> {
    if let Some(length) = response.content_length() {
        if length > RESPONSE_BODY_CAP as u64 {
            return Err(llm_err(
                "http",
                format!("provider response exceeds the {RESPONSE_BODY_CAP}-byte limit"),
            ));
        }
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| llm_err("http", format!("failed to read response: {e}")))?
    {
        if body.len() + chunk.len() > RESPONSE_BODY_CAP {
            return Err(llm_err(
                "http",
                format!("provider response exceeds the {RESPONSE_BODY_CAP}-byte limit"),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|e| llm_err("http", format!("failed to parse response: {e}")))
}

async fn do_llm(mut messages: Vec<serde_json::Value>) -> Result<String, rquickjs::Error> {
    let key = std::env::var("TERRARIUM_LLM_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(llm_err(
            "env",
            "TERRARIUM_LLM_API_KEY not set; host.llm unavailable".into(),
        ));
    }
    let model = model_name();

    // concurrency gate: the cage heap and the run deadline already bound everything else; the
    // network gets the same treatment (Promise.all fanouts otherwise hit fd limits at the provider)
    let _permit = LLM_GATE.acquire().await.expect("llm gate never closes");

    LLM_CALLS.fetch_add(1, Ordering::Relaxed);
    // 120s per-request timeout + 1 retry: hangs self-heal instead of eating the whole turn budget.
    // The retry is keyed to error class: transport/5xx/429 may heal, a 4xx won't — its body says why.
    let mut resp: Option<serde_json::Value> = None;
    let mut last_err = String::new();
    let mut no_retry = false;
    for attempt in 0..2 {
        let r = http_client()
            .post(base_url())
            .bearer_auth(&key)
            .json(&serde_json::json!({ "model": model, "messages": messages, "stream": false }))
            .send()
            .await;
        match r {
            Ok(rs) => {
                let status = rs.status();
                if !status.is_success() {
                    last_err = format!("HTTP {status}");
                    if status.is_client_error() && status.as_u16() != 429 {
                        no_retry = true; // bad request/auth won't heal — surface it now
                    }
                } else {
                    match read_json_response(rs).await {
                        Ok(v) => {
                            resp = Some(v);
                            break;
                        }
                        Err(e) => last_err = e.to_string(),
                    }
                }
            }
            Err(e) => last_err = format!("request failed: {e}"),
        }
        if attempt == 0 && !no_retry {
            tokio::time::sleep(Duration::from_secs(2)).await;
            LLM_CALLS.fetch_add(1, Ordering::Relaxed);
        } else {
            break;
        }
    }
    let resp = match resp {
        Some(v) => v,
        None => {
            return Err(llm_err(
                "http",
                format!("still failing after retry: {last_err}"),
            ))
        }
    };
    messages.clear();

    if let Some(u) = resp.get("usage") {
        LLM_CACHE_HIT.fetch_add(
            u.get("prompt_cache_hit_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        LLM_CACHE_MISS.fetch_add(
            u.get("prompt_cache_miss_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        LLM_OUT_TOKENS.fetch_add(
            u.get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
    }
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| llm_err("response", "response missing text content".into()))?;
    Ok(content.to_string())
}

/// Registers the host.llm namespace; contract holds the full contract text (chat auto-prepends it to system)
pub fn install<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    contract: &str,
    cancel_tx: &watch::Sender<bool>,
) -> rquickjs::Result<()> {
    let llm_ns = Object::new(ctx.clone())?;

    let cancel0 = cancel_tx.subscribe();
    let call_fn = Function::new(
        ctx.clone(),
        Async(move |prompt: String, system: Opt<String>| {
            let mut cancel = cancel0.clone();
            async move {
                let mut messages = Vec::new();
                if let Some(s) = system.0 {
                    messages.push(serde_json::json!({ "role": "system", "content": s }));
                }
                messages.push(serde_json::json!({ "role": "user", "content": prompt }));
                tokio::select! {
                    r = do_llm(messages) => r,
                    _ = cancel.changed() => Err(llm_err("cancel", "deadline: host.llm call cancelled".into())),
                }
            }
        }),
    )?;
    llm_ns.set("call", call_fn)?;

    let cancel1 = cancel_tx.subscribe();
    let contract = contract.to_string();
    let chat_fn = Function::new(
        ctx.clone(),
        Async(move |messages: Vec<ChatMsg>, system: Opt<String>| {
            let mut cancel = cancel1.clone();
            let contract = contract.clone(); // clone per call, then move into the future (keeps the closure Fn)
            async move {
                // system prompt = standard contract (environment facts) + task-specific part
                let sys = match system.0 {
                    Some(s) if !s.trim().is_empty() => format!("{contract}\n\n# Task\n{s}"),
                    _ => contract,
                };
                let mut msgs = vec![serde_json::json!({ "role": "system", "content": sys })];
                for m in &messages {
                    msgs.push(serde_json::json!({ "role": m.role, "content": m.content }));
                }
                tokio::select! {
                    r = do_llm(msgs) => r,
                    _ = cancel.changed() => Err(llm_err("cancel", "deadline: host.llm call cancelled".into())),
                }
            }
        }),
    )?;
    llm_ns.set("chat", chat_fn)?;

    host.set("llm", llm_ns)
}

/// Usage summary embedded in the result JSON (per process lifetime)
pub fn usage_json() -> serde_json::Value {
    serde_json::json!({
        "calls": LLM_CALLS.load(Ordering::Relaxed),
        "cache_hit_tokens": LLM_CACHE_HIT.load(Ordering::Relaxed),
        "cache_miss_tokens": LLM_CACHE_MISS.load(Ordering::Relaxed),
        "output_tokens": LLM_OUT_TOKENS.load(Ordering::Relaxed),
    })
}

#[cfg(test)]
mod tests {
    use super::{capability_text, model_capabilities, Modality, RESPONSE_BODY_CAP};

    #[test]
    fn known_models_declare_input_capabilities() {
        let text = model_capabilities()
            .iter()
            .find(|model| model.id == "deepseek-v4-flash")
            .expect("text model");
        assert_eq!(text.input_modalities, &[Modality::Text]);

        let vision = model_capabilities()
            .iter()
            .find(|model| model.id == "deepseek-v4-flash-vision-exp")
            .expect("vision model");
        assert_eq!(vision.input_modalities, &[Modality::Text, Modality::Image]);
        assert_eq!(vision.output_modalities, &[Modality::Text]);
    }

    #[test]
    fn capability_text_warns_that_image_payloads_are_not_implemented() {
        let text = capability_text("deepseek-v4-flash-vision-exp");
        assert!(text.contains("declared input: text, image"));
        assert!(text.contains("payloads are text-only"));
    }

    #[test]
    fn response_body_cap_is_finite() {
        assert_eq!(RESPONSE_BODY_CAP, 4 * 1024 * 1024);
    }
}
