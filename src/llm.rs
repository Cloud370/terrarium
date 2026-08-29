use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Serialize;

use crate::config::ResolvedProfile;

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
            "model_id: {model} (capabilities are not declared locally; requests remain text-only)"
        );
    };
    let inputs = capability
        .input_modalities
        .iter()
        .map(|m| match m {
            Modality::Text => "text",
            Modality::Image => "image",
        })
        .collect::<Vec<_>>()
        .join(", ");
    let outputs = capability
        .output_modalities
        .iter()
        .map(|m| match m {
            Modality::Text => "text",
            Modality::Image => "image",
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("model_id: {model} (declared input: {inputs}; declared output: {outputs}; current requests are text-only)")
}

static LLM_CALLS: AtomicU64 = AtomicU64::new(0);
static LLM_CACHE_HIT: AtomicU64 = AtomicU64::new(0);
static LLM_CACHE_MISS: AtomicU64 = AtomicU64::new(0);
static LLM_OUT_TOKENS: AtomicU64 = AtomicU64::new(0);

fn http_client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("http client")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmError {
    pub kind: &'static str,
    pub message: String,
    pub retryable: bool,
}

async fn read_json_response(
    mut response: reqwest::Response,
) -> Result<serde_json::Value, LlmError> {
    if response
        .content_length()
        .is_some_and(|n| n > RESPONSE_BODY_CAP as u64)
    {
        return Err(LlmError {
            kind: "http",
            message: format!("provider response exceeds the {RESPONSE_BODY_CAP}-byte limit"),
            retryable: false,
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| LlmError {
        kind: "transport",
        message: format!("failed to read response: {e}"),
        retryable: true,
    })? {
        if body.len() + chunk.len() > RESPONSE_BODY_CAP {
            return Err(LlmError {
                kind: "http",
                message: format!("provider response exceeds the {RESPONSE_BODY_CAP}-byte limit"),
                retryable: false,
            });
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|e| LlmError {
        kind: "protocol",
        message: format!("failed to parse response: {e}"),
        retryable: false,
    })
}

pub async fn complete(
    profile: &ResolvedProfile,
    messages: Vec<serde_json::Value>,
) -> Result<String, LlmError> {
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
    let url = format!(
        "{}/chat/completions",
        profile.base_url.trim_end_matches('/')
    );
    let mut payload =
        serde_json::json!({ "model": profile.model, "messages": messages, "stream": false });
    if let Some(value) = profile.max_output_tokens {
        payload["max_tokens"] = serde_json::json!(value);
    }
    if let Some(value) = &profile.reasoning_effort {
        payload["reasoning_effort"] = serde_json::json!(value);
    }
    LLM_CALLS.fetch_add(1, Ordering::Relaxed);
    let mut request = http_client().post(url).json(&payload);
    if let Some(key) = key.filter(|k| !k.is_empty()) {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|e| LlmError {
        kind: "transport",
        message: format!("request failed: {e}"),
        retryable: true,
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(LlmError {
            kind: "http",
            message: format!("HTTP {status}"),
            retryable: status.as_u16() == 429 || status.is_server_error(),
        });
    }
    let response = read_json_response(response).await?;
    if let Some(usage) = response.get("usage") {
        LLM_CACHE_HIT.fetch_add(
            usage
                .get("prompt_cache_hit_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        LLM_CACHE_MISS.fetch_add(
            usage
                .get("prompt_cache_miss_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        LLM_OUT_TOKENS.fetch_add(
            usage
                .get("completion_tokens")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
    }
    response
        .get("choices")
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("message"))
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| LlmError {
            kind: "protocol",
            message: "response missing text content".into(),
            retryable: false,
        })
}

pub fn usage_json() -> serde_json::Value {
    serde_json::json!({ "calls": LLM_CALLS.load(Ordering::Relaxed), "cache_hit_tokens": LLM_CACHE_HIT.load(Ordering::Relaxed), "cache_miss_tokens": LLM_CACHE_MISS.load(Ordering::Relaxed), "output_tokens": LLM_OUT_TOKENS.load(Ordering::Relaxed) })
}

#[cfg(test)]
mod tests {
    use super::{capability_text, model_capabilities, Modality};
    #[test]
    fn known_models_declare_input_capabilities() {
        let text = model_capabilities()
            .iter()
            .find(|model| model.id == "deepseek-v4-flash")
            .unwrap();
        assert_eq!(text.input_modalities, &[Modality::Text]);
        let vision = model_capabilities()
            .iter()
            .find(|model| model.id == "deepseek-v4-flash-vision-exp")
            .unwrap();
        assert_eq!(vision.input_modalities, &[Modality::Text, Modality::Image]);
        assert_eq!(vision.output_modalities, &[Modality::Text]);
    }
    #[test]
    fn capability_text_warns_that_image_payloads_are_not_implemented() {
        let text = capability_text("deepseek-v4-flash-vision-exp");
        assert!(text.contains("model_id: deepseek-v4-flash-vision-exp"));
        assert!(text.contains("declared input: text, image"));
        assert!(text.contains("requests are text-only"));
    }
}
