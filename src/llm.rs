//! host.llm —— nested model calls: call (one-shot) / chat (multi-turn, auto-prepends contract).
//! Async-concurrent (Promise.all = max not sum), cancellable (watch token), transport hangs self-heal via 120s × 2 retries.
//! Keys live only in this process's env; the sandbox can't see them. (usage/contract introspection cut in D017 —
//! Rust-side usage_snapshot below still feeds the operator's stderr stats.)

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use rquickjs::function::{Async, Opt};
use rquickjs::{Ctx, Function, Object};
use tokio::sync::watch;

static LLM_CALLS: AtomicU64 = AtomicU64::new(0);
static LLM_CACHE_HIT: AtomicU64 = AtomicU64::new(0);
static LLM_CACHE_MISS: AtomicU64 = AtomicU64::new(0);
static LLM_OUT_TOKENS: AtomicU64 = AtomicU64::new(0);

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
        Ok(ChatMsg {
            role: obj.get("role")?,
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

/// Endpoint seam: provider is env-overridable so the loop isn't welded to one vendor (default = DeepSeek).
fn base_url() -> String {
    std::env::var("TERRARIUM_LLM_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com/chat/completions".into())
}

/// Model identity — single source (env-overridable); both do_llm and the MAIN role layer fill from here
pub fn model_name() -> String {
    std::env::var("TERRARIUM_LLM_MODEL").unwrap_or_else(|_| "deepseek-v4-flash-vision-exp".into())
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

/// Atomic snapshot for per-round deltas (outer-ring usage = delta across one outer call; sub-agent calls happen between them)
pub fn usage_snapshot() -> (u64, u64, u64, u64) {
    (
        LLM_CALLS.load(Ordering::Relaxed),
        LLM_CACHE_HIT.load(Ordering::Relaxed),
        LLM_CACHE_MISS.load(Ordering::Relaxed),
        LLM_OUT_TOKENS.load(Ordering::Relaxed),
    )
}

fn llm_err(from: &'static str, msg: String) -> rquickjs::Error {
    rquickjs::Error::FromJs {
        from,
        to: "host.llm",
        message: Some(msg),
    }
}

async fn do_llm(mut messages: Vec<serde_json::Value>) -> Result<String, rquickjs::Error> {
    let key = std::env::var("DEEPSEEK_API_KEY").unwrap_or_default();
    if key.is_empty() {
        return Err(llm_err(
            "env",
            "DEEPSEEK_API_KEY not set; host.llm unavailable".into(),
        ));
    }
    let model = model_name();

    LLM_CALLS.fetch_add(1, Ordering::Relaxed);
    // 120s per-request timeout + 1 retry: hangs self-heal instead of eating the whole turn budget
    let mut resp: Option<serde_json::Value> = None;
    let mut last_err = String::new();
    for attempt in 0..2 {
        let r = http_client()
            .post(base_url())
            .bearer_auth(&key)
            .json(&serde_json::json!({ "model": model, "messages": messages, "stream": false }))
            .send()
            .await;
        match r {
            Ok(rs) => match rs.json::<serde_json::Value>().await {
                Ok(v) => {
                    resp = Some(v);
                    break;
                }
                Err(e) => last_err = format!("failed to parse response: {e}"),
            },
            Err(e) => last_err = format!("request failed: {e}"),
        }
        if attempt == 0 {
            tokio::time::sleep(Duration::from_secs(2)).await;
            LLM_CALLS.fetch_add(1, Ordering::Relaxed);
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
        .ok_or_else(|| {
            llm_err(
                "response",
                format!(
                    "response missing content: {}",
                    resp.to_string().chars().take(300).collect::<String>()
                ),
            )
        })?;
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
