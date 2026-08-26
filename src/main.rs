//! terrarium —— an LLM's actions are programs: one ES2020 program in, one JSON result out.
//! Foundation = async kernel + cancellation protocol + host.fs/host.llm + teaching contract (static assets).
//! Layout: main.rs (pipeline) / registry.rs (registry) / fs.rs / llm.rs / CONTRACT.md / prelude.js

mod agent;
mod fs;
mod llm;
mod registry;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Function, Object, Value};
use tokio::sync::watch;

const MEM_LIMIT: usize = 64 * 1024 * 1024;
const STACK_LIMIT: usize = 1024 * 1024;
const STDOUT_CAP: usize = 16 * 1024;
const GRACE_MS: u64 = 2000; // wind-down window granted to the cancellation protocol after timeout

const PRELUDE: &str = include_str!("prelude.js");
const CONTRACT_TEMPLATE: &str = include_str!("CONTRACT.md");

/// Contract = static teaching template + registry-generated API surface + actual mounts. Main/sub agents share one copy.
pub(crate) fn contract(mounts: &[fs::Mount]) -> String {
    let ro: Vec<String> = mounts
        .iter()
        .filter(|m| !m.rw)
        .map(|m| m.virt.trim_end_matches('/').to_string())
        .collect();
    let rw: Vec<String> = mounts
        .iter()
        .filter(|m| m.rw)
        .map(|m| m.virt.trim_end_matches('/').to_string())
        .collect();
    let mut mounts_line = String::new();
    if !ro.is_empty() {
        mounts_line.push_str(&format!(
            "Read-only mounts: {} (`host.fs.search` skips `target/`, `.git/`, `node_modules/`, `ref/` by default — override with its `skips` argument)\n",
            ro.join(", ")
        ));
    }
    if !rw.is_empty() {
        mounts_line.push_str(&format!(
            "Writable mounts: {} — `host.fs.write` is allowed here and only here (atomic, auto-creates parent dirs)",
            rw.join(", ")
        ));
    }
    CONTRACT_TEMPLATE
        .replace("{{HOST_API}}", registry::api_lines().trim_end())
        .replace("{{MOUNTS}}", mounts_line.trim_end())
}

/// Output-channel discrimination: a line starting with return/await → execute as an async function body
/// (top-level `return` submits, top-level `await` is legal); otherwise run straight as a script (last expression).
/// Misjudging the direction just wraps one extra layer; behavior unchanged.
fn wants_function_body(code: &str) -> bool {
    code.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("return") || t.starts_with("await ") || t == "await"
    })
}

pub(crate) struct Outcome {
    pub(crate) ok: bool,
    pub(crate) result: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) stdout: String,
    pub(crate) timed_out: bool,
    pub(crate) elapsed_ms: u128,
}

fn internal(msg: String) -> Outcome {
    Outcome {
        ok: false,
        result: None,
        error: Some(msg),
        stdout: String::new(),
        timed_out: false,
        elapsed_ms: 0,
    }
}

/// One call = one fresh cage (new Runtime/Context + limits + cancellation). The agent loop reuses this per run;
/// conversation state stays outside so a dead run never takes the session with it.
pub(crate) async fn eval_js(
    code: &str,
    timeout_ms: u64,
    mounts: &[fs::Mount],
    cancel_tx: watch::Sender<bool>,
) -> Outcome {
    let start = Instant::now();
    let deadline = start + Duration::from_millis(timeout_ms);
    let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let rt = match AsyncRuntime::new() {
        Ok(rt) => rt,
        Err(e) => return internal(format!("runtime init failed: {e:?}")),
    };
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
        .await;
    rt.set_memory_limit(MEM_LIMIT).await;
    rt.set_max_stack_size(STACK_LIMIT).await;
    let ctx = match AsyncContext::full(&rt).await {
        Ok(ctx) => ctx,
        Err(e) => return internal(format!("context init failed: {e:?}")),
    };

    let contract = contract(mounts);
    let body = ctx.async_with(async |ctx| -> rquickjs::Result<Outcome> {
        let sink = logs.clone();
        let log_fn = Function::new(ctx.clone(), move |s: String| sink.borrow_mut().push(s))?;
        ctx.globals().set("__log", log_fn)?;

        let host = Object::new(ctx.clone())?;
        registry::install(&ctx, &host)?;
        llm::install(&ctx, &host, &contract, &cancel_tx)?;
        fs::install(&ctx, &host, mounts)?;
        // protocol single-source: prelude's spawnAgent delegates reply→code extraction here (agent::extract_run);
        // internal helper, deliberately not part of the documented HOST_API surface
        let extract_fn = Function::new(ctx.clone(), |reply: String| {
            agent::extract_run(&reply).map(|b| b.code)
        })?;
        host.set("__extractRun", extract_fn)?;
        ctx.globals().set("host", host)?;
        ctx.eval::<Value, _>(PRELUDE)?;

        let mut out = Outcome {
            ok: true,
            result: None,
            error: None,
            stdout: String::new(),
            timed_out: false,
            elapsed_ms: 0,
        };
        let source = if wants_function_body(code) {
            format!("(async () => {{\n{code}\n}})()")
        } else {
            code.to_string()
        };

        match ctx.eval::<Value, _>(source.as_str()).catch(&ctx) {
            Ok(v) => {
                if v.is_promise() {
                    // trailing Promise: tap it onto the observer and await settlement
                    let tap: Function = ctx.globals().get("__tap")?;
                    let tapped: Value = tap.call((v,))?;
                    let p = tapped
                        .into_promise()
                        .ok_or_else(|| rquickjs::Error::FromJs {
                            from: "tap",
                            to: "promise",
                            message: Some("internal: tapped not promise".into()),
                        })?;
                    p.into_future::<Value>().await?;
                    let ok: bool = ctx.eval::<bool, _>("__settled.ok")?;
                    if ok {
                        out.result = ctx.eval::<Option<String>, _>(
                            "__settled.v === undefined ? null : __fmt(__settled.v)",
                        )?;
                    } else {
                        out.ok = false;
                        out.error = ctx.eval::<Option<String>, _>(
                            "__settled.e === undefined ? null : String(__settled.e)",
                        )?;
                    }
                } else {
                    ctx.globals().set("__res", v)?;
                    out.result =
                        ctx.eval::<Option<String>, _>("__res === undefined ? null : __fmt(__res)")?;
                }
            }
            Err(CaughtError::Exception(ex)) => {
                let msg = ex.message().unwrap_or_default();
                let stack = ex.stack().unwrap_or_default();
                if msg == "interrupted" {
                    out.timed_out = true;
                }
                out.ok = false;
                out.error = Some(if stack.is_empty() {
                    msg
                } else {
                    format!("{msg}\n{stack}")
                });
            }
            Err(CaughtError::Value(v)) => {
                ctx.globals().set("__ex", v)?;
                out.ok = false;
                out.error = Some(ctx.eval::<String, _>("String(__ex)")?);
            }
            Err(CaughtError::Error(e)) => {
                out.ok = false;
                out.error = Some(format!("internal error: {e:?}"));
            }
        }
        Ok(out)
    });

    // Cancellation protocol (the shape validated in cancel-demo): deadline hit → send cancel token → keep driving wind-down → normal destructors
    let mut body = Box::pin(body);
    let timer = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
    tokio::pin!(timer);
    let out_opt = tokio::select! {
        r = &mut body => Some(r.unwrap_or_else(|e| internal(format!("internal: {e:?}")))),
        _ = &mut timer => {
            cancel_tx.send_replace(true);
            match tokio::time::timeout(Duration::from_millis(GRACE_MS), &mut body).await {
                Ok(r) => {
                    let mut o = r.unwrap_or_else(|e| internal(format!("internal: {e:?}")));
                    o.timed_out = true;
                    Some(o)
                }
                Err(_) => None, // grace wasn't enough (shouldn't happen in theory)
            }
        }
    };
    drop(body); // release the future's borrow of ctx before tearing down ctx/rt

    let mut out = match out_opt {
        Some(o) => o,
        None => {
            std::mem::forget((rt, ctx)); // leak the runtime to keep the process alive (same lifetime as process exit)
            Outcome {
                ok: false,
                result: None,
                error: Some("grace exceeded".into()),
                stdout: logs.borrow().join("\n"),
                timed_out: true,
                elapsed_ms: 0,
            }
        }
    };

    out.elapsed_ms = start.elapsed().as_millis();
    if start.elapsed() >= Duration::from_millis(timeout_ms) {
        out.timed_out = true;
    }
    let mut joined = logs.borrow().join("\n");
    if joined.len() > STDOUT_CAP {
        joined = format!("{}\n...[truncated]", &joined[..STDOUT_CAP]);
    }
    out.stdout = joined;
    out
}

/// Shared mount parsing for both entry modes (single run / agent): `virt=real` (read-only) or `virt=real:rw`.
/// A real dir literally named "…:rw" (legal on Unix) still mounts: existence disambiguates — if the stripped
/// path is missing but the full spec exists, the dirname itself ends in ":rw" and the mount is read-only.
fn add_mount(mounts: &mut Vec<fs::Mount>, spec: &str) {
    let Some((virt, real_spec)) = spec.split_once('=') else {
        eprintln!("ignoring invalid mount spec {spec} (expected /virtual-prefix=real-path[:rw])");
        return;
    };
    let (real, rw) = match real_spec.strip_suffix(":rw") {
        Some(r) => (r, true),
        None => (real_spec, false),
    };
    let mount = |root, rw| fs::Mount {
        virt: format!("{}/", virt.trim_end_matches('/')),
        root,
        rw,
    };
    match std::fs::canonicalize(real) {
        Ok(root) => mounts.push(mount(root, rw)),
        Err(_) => match std::fs::canonicalize(real_spec) {
            Ok(root) => {
                if rw {
                    eprintln!("note: {real} not found — mounting {real_spec} read-only (dirname ends in :rw)");
                }
                mounts.push(mount(root, false));
            }
            Err(e) => eprintln!("ignoring invalid mount {spec}: {e}"),
        },
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("agent") {
        std::process::exit(agent::run_cli(&args[1..]).await);
    }
    let mut timeout_ms: u64 = 2000;
    let mut code = String::new();
    let mut mounts: Vec<fs::Mount> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--timeout" {
            timeout_ms = args[i + 1].parse().unwrap_or(2000);
            i += 2;
        } else if args[i] == "--contract" {
            // print the standard contract; outer agent loop consumes it — single source of truth
            print!("{}", contract(&mounts));
            return;
        } else if args[i] == "--mount" {
            add_mount(&mut mounts, &args[i + 1]);
            i += 2;
        } else {
            if !code.is_empty() {
                code.push(' ');
            }
            code.push_str(&args[i]);
            i += 1;
        }
    }
    if code.is_empty() {
        code = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
    }

    let (cancel_tx, _cancel_rx) = watch::channel(false);
    let out = eval_js(&code, timeout_ms, &mounts, cancel_tx).await;
    println!(
        "{}",
        serde_json::json!({
            "ok": out.ok,
            "result": out.result,
            "stdout": out.stdout,
            "error": out.error,
            "timed_out": out.timed_out,
            "elapsed_ms": out.elapsed_ms,
            "target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            "limits": { "memory": "64MB", "stack": "1MB", "timeout_ms": timeout_ms },
            "mounts": mounts.iter().map(|m| m.virt.clone()).collect::<Vec<_>>(),
            "llm_usage": llm::usage_json(),
        })
    );
}
