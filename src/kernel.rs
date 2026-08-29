//! One Terrarium run: a fresh QuickJS cage, installed host capabilities, hard limits, and a
//! structured outcome. Terminal I/O and process exit codes belong to `cli`, not this module.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rquickjs::{AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Function, Object, Value};
use serde::Serialize;
use tokio::sync::watch;

use crate::{fs, llm, registry};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteSummary {
    pub path: String,
    pub created: bool,
    pub changed: bool,
    pub bytes_before: Option<u64>,
    pub bytes_after: u64,
    pub first_changed_line: Option<usize>,
}

/// One physics, enforced on both sides of the cage wall: the host refuses reads bigger than the
/// heap the cage could hold anyway (fs.rs), so "bounded" never depends on which side allocates first.
pub(crate) const MEM_LIMIT: usize = 64 * 1024 * 1024;
const STACK_LIMIT: usize = 1024 * 1024;
const STDOUT_CAP: usize = 16 * 1024;
const GRACE_MS: u64 = 2000;
pub(crate) const MAX_TIMEOUT_MS: u64 = 300_000;
pub(crate) const VALUE_CAP: usize = 24 * 1024;
pub(crate) const FACTS_CAP: usize = 16 * 1024;

const PRELUDE: &str = include_str!("runtime/prelude.js");
const CONTRACT_TEMPLATE: &str = include_str!("prompts/contract.md");

static LEAKED_RUNTIMES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Char-boundary-safe truncation with a visible marker.
pub(crate) fn truncate_utf8(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

fn mount_label(mount: &fs::Mount) -> String {
    if mount.virt == "/" {
        "/".into()
    } else {
        mount.virt.trim_end_matches('/').to_string()
    }
}

/// Contract = static teaching template + registry-generated API surface + actual mounts.
pub(crate) fn contract(mounts: &[fs::Mount]) -> String {
    contract_for(mounts, "configured model")
}

pub(crate) fn contract_for(mounts: &[fs::Mount], model: &str) -> String {
    let ro: Vec<String> = mounts.iter().filter(|m| !m.rw).map(mount_label).collect();
    let rw: Vec<String> = mounts.iter().filter(|m| m.rw).map(mount_label).collect();
    let mut mounts_line = String::new();
    mounts_line.push_str(&format!("{}\n", llm::capability_text(model)));
    if !ro.is_empty() {
        mounts_line.push_str(&format!(
            "Read-only mounts: {} (`host.fs.scan` runs with ripgrep's defaults here — .gitignore respected, \
             dot-entries skipped, binaries detected by content; override per call with `{{gitignore: false}}`/`{{hidden: true}}`)\n",
            ro.join(", ")
        ));
    }
    if !rw.is_empty() {
        mounts_line.push_str(&format!(
            "Writable mounts: {} — `host.fs.write` is allowed here and only here (atomic, auto-creates parent dirs; the run result includes host-derived write receipts)",
            rw.join(", ")
        ));
    }
    CONTRACT_TEMPLATE
        .replace("{{HOST_API}}", registry::api_lines().trim_end())
        .replace("{{MOUNTS}}", mounts_line.trim_end())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Termination {
    Returned,
    Failed,
    TimedOut,
    Cancelled,
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Runtime,
    Capability,
    Configuration,
    Internal,
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunError {
    pub kind: ErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    pub ok: bool,
    pub value: Option<serde_json::Value>,
    pub error: Option<RunError>,
    pub termination: Termination,
    pub stdout: String,
    pub timed_out: bool,
    pub elapsed_ms: u64,
    pub writes: Vec<WriteSummary>,
    pub writes_truncated: bool,
}

fn failure(kind: ErrorKind, msg: impl Into<String>) -> Outcome {
    Outcome {
        ok: false,
        value: None,
        error: Some(RunError {
            kind,
            message: msg.into(),
        }),
        termination: Termination::Fatal,
        stdout: String::new(),
        timed_out: false,
        elapsed_ms: 0,
        writes: Vec::new(),
        writes_truncated: false,
    }
}

fn invalid_timeout(timeout_ms: u64) -> Outcome {
    failure(
        ErrorKind::Configuration,
        format!("timeout must be between 1 and {MAX_TIMEOUT_MS} ms (got {timeout_ms})"),
    )
}

fn value_to_json<'js>(
    ctx: &rquickjs::Ctx<'js>,
    value: Value<'js>,
) -> rquickjs::Result<Option<serde_json::Value>> {
    if value.is_undefined() {
        return Ok(None);
    }
    let Some(json) = ctx.json_stringify(value)? else {
        return Err(rquickjs::Error::FromJs {
            from: "value",
            to: "JSON",
            message: Some("result is not JSON serializable".into()),
        });
    };
    let text = json.to_string()?;
    if text.len() > VALUE_CAP {
        return Err(rquickjs::Error::FromJs {
            from: "value",
            to: "JSON",
            message: Some(format!("result exceeds the {VALUE_CAP}-byte limit")),
        });
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| rquickjs::Error::FromJs {
            from: "value",
            to: "JSON",
            message: Some(format!("result is not valid JSON: {e}")),
        })
}

/// One call = one fresh cage. Conversation state stays outside so a dead run never takes the session with it.
pub(crate) async fn eval_js(
    code: &str,
    timeout_ms: u64,
    mounts: &[fs::Mount],
    cancel_tx: watch::Sender<bool>,
) -> Outcome {
    if !(1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return invalid_timeout(timeout_ms);
    }
    let start = Instant::now();
    let Some(deadline) = start.checked_add(Duration::from_millis(timeout_ms)) else {
        return invalid_timeout(timeout_ms);
    };
    let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let overflowed: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
    let writes: Rc<RefCell<fs::WriteLog>> = Rc::new(RefCell::new(fs::WriteLog::default()));

    let rt = match AsyncRuntime::new() {
        Ok(rt) => rt,
        Err(e) => return failure(ErrorKind::Internal, format!("runtime init failed: {e:?}")),
    };
    rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)))
        .await;
    rt.set_memory_limit(MEM_LIMIT).await;
    rt.set_max_stack_size(STACK_LIMIT).await;
    let ctx = match AsyncContext::full(&rt).await {
        Ok(ctx) => ctx,
        Err(e) => return failure(ErrorKind::Internal, format!("context init failed: {e:?}")),
    };

    let body = ctx.async_with(async |ctx| -> rquickjs::Result<Outcome> {
        let sink = logs.clone();
        let overflowed = overflowed.clone();
        let used = std::cell::Cell::new(0usize);
        let log_fn = Function::new(ctx.clone(), move |s: String| {
            let used_now = used.get();
            if used_now >= STDOUT_CAP {
                overflowed.set(true);
                return;
            }
            let remaining = STDOUT_CAP - used_now;
            if s.len() + 1 > remaining {
                sink.borrow_mut()
                    .push(truncate_utf8(&s, remaining.saturating_sub(1)));
                used.set(STDOUT_CAP);
                overflowed.set(true);
            } else {
                used.set(used_now + s.len() + 1);
                sink.borrow_mut().push(s);
            }
        })?;
        ctx.globals().set("__log", log_fn)?;

        let host = Object::new(ctx.clone())?;
        fs::install(&ctx, &host, mounts, writes.clone())?;
        ctx.globals().set("host", host)?;
        ctx.eval::<Value, _>(PRELUDE)?;

        let mut out = Outcome {
            ok: true,
            value: None,
            error: None,
            termination: Termination::Returned,
            stdout: String::new(),
            timed_out: false,
            elapsed_ms: 0,
            writes: Vec::new(),
            writes_truncated: false,
        };
        let source = format!("(async () => {{\n{code}\n}})()");
        let result = match ctx.eval::<Value, _>(source.as_str()).catch(&ctx) {
            Ok(v) => {
                if v.is_promise() {
                    let p = v.into_promise().ok_or_else(|| rquickjs::Error::FromJs {
                        from: "value",
                        to: "promise",
                        message: Some("internal: promise conversion failed".into()),
                    })?;
                    p.into_future::<Value>()
                        .await
                        .map_err(|error| CaughtError::from_error(&ctx, error))
                } else {
                    Ok(v)
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(value) => {
                out.value = value_to_json(&ctx, value)?;
            }
            Err(CaughtError::Exception(ex)) => {
                let msg = ex.message().unwrap_or_default();
                let stack = ex.stack().unwrap_or_default();
                out.ok = false;
                if msg == "interrupted" {
                    out.termination = Termination::TimedOut;
                    out.timed_out = true;
                    out.error = Some(RunError {
                        kind: ErrorKind::Runtime,
                        message: "deadline exceeded".into(),
                    });
                } else {
                    out.termination = Termination::Failed;
                    out.error = Some(RunError {
                        kind: ErrorKind::Runtime,
                        message: if stack.is_empty() {
                            msg
                        } else {
                            format!("{msg}\n{stack}")
                        },
                    });
                }
            }
            Err(CaughtError::Value(v)) => {
                ctx.globals().set("__ex", v)?;
                out.ok = false;
                out.termination = Termination::Failed;
                out.error = Some(RunError {
                    kind: ErrorKind::Runtime,
                    message: ctx.eval::<String, _>("String(__ex)")?,
                });
            }
            Err(CaughtError::Error(e)) => {
                out.ok = false;
                out.termination = Termination::Fatal;
                out.error = Some(RunError {
                    kind: ErrorKind::Internal,
                    message: format!("internal error: {e:?}"),
                });
            }
        }
        Ok(out)
    });

    let mut body = Box::pin(body);
    let timer = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
    tokio::pin!(timer);
    let out_opt = tokio::select! {
        r = &mut body => Some(r.unwrap_or_else(|e| failure(ErrorKind::Internal, format!("internal: {e:?}")))),
        _ = &mut timer => {
            cancel_tx.send_replace(true);
            match tokio::time::timeout(Duration::from_millis(GRACE_MS), &mut body).await {
                Ok(r) => {
                    let mut o = r.unwrap_or_else(|e| failure(ErrorKind::Internal, format!("internal: {e:?}")));
                    o.ok = false;
                    o.timed_out = true;
                    o.termination = Termination::TimedOut;
                    o.value = None;
                    o.error = Some(RunError { kind: ErrorKind::Runtime, message: "deadline exceeded".into() });
                    Some(o)
                }
                Err(_) => None,
            }
        }
    };
    drop(body);

    let mut out = match out_opt {
        Some(o) => o,
        None => {
            let leaked = LEAKED_RUNTIMES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            std::mem::forget((rt, ctx));
            eprintln!("[terrarium] run hung past the grace window — runtime leaked ({leaked})");
            failure(
                ErrorKind::Internal,
                "grace window exceeded while cancelling the run",
            )
        }
    };

    out.elapsed_ms = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
    if out.termination == Termination::Returned && out.elapsed_ms >= timeout_ms {
        out.ok = false;
        out.value = None;
        out.timed_out = true;
        out.termination = Termination::TimedOut;
        out.error = Some(RunError {
            kind: ErrorKind::Runtime,
            message: "deadline exceeded".into(),
        });
    }
    let mut joined = logs.borrow().join("\n");
    if overflowed.get() {
        joined.push_str("\n…[truncated]");
    }
    out.stdout = truncate_utf8(&joined, STDOUT_CAP);
    {
        let mut write_log = writes.borrow_mut();
        out.writes = std::mem::take(&mut write_log.items);
        out.writes_truncated = write_log.truncated;
    }
    out
}

/// Shared mount parsing for both entry modes.
pub(crate) fn add_mount(mounts: &mut Vec<fs::Mount>, spec: &str) -> Result<(), String> {
    let Some((virt, real_spec)) = spec.split_once('=') else {
        return Err(format!(
            "invalid mount spec {spec} (expected /virtual-prefix=real-path[:rw])"
        ));
    };
    let virt = virt.trim_end_matches('/');
    let (real, rw) = match real_spec.strip_suffix(":rw") {
        Some(r) => (r, true),
        None => (real_spec, false),
    };
    let root = match std::fs::canonicalize(real) {
        Ok(root) => root,
        Err(_) if rw => {
            std::fs::canonicalize(real_spec).map_err(|e| format!("invalid mount {spec}: {e}"))?
        }
        Err(e) => return Err(format!("invalid mount {spec}: {e}")),
    };
    let mount = fs::Mount::from_canonical(virt, root, rw)?;
    if mounts.iter().any(|m| m.overlaps(&mount)) {
        return Err(format!(
            "overlapping mount virtual roots are not allowed: {virt}"
        ));
    }
    mounts.push(mount);
    Ok(())
}

/// Reusable execution facade for non-CLI frontends.
#[derive(Debug, Clone)]
pub struct Kernel {
    mounts: Vec<fs::Mount>,
}

impl Kernel {
    pub fn new(mut mounts: Vec<fs::Mount>) -> Result<Self, String> {
        for (index, mount) in mounts.iter().enumerate() {
            if mounts[index + 1..]
                .iter()
                .any(|other| mount.overlaps(other))
            {
                return Err(format!(
                    "overlapping mount virtual roots are not allowed: {}",
                    mount.virt.trim_end_matches('/')
                ));
            }
        }
        mounts.sort_by_key(|m| std::cmp::Reverse(m.virt.len()));
        Ok(Self { mounts })
    }

    pub fn mounts(&self) -> &[fs::Mount] {
        &self.mounts
    }

    pub fn contract(&self) -> String {
        contract(&self.mounts)
    }

    pub async fn run(&self, code: &str, timeout_ms: u64) -> Outcome {
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        eval_js(code, timeout_ms, &self.mounts, cancel_tx).await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{add_mount, contract_for, eval_js, truncate_utf8, ErrorKind, Termination};
    use tokio::sync::watch;

    #[test]
    fn truncate_utf8_lands_on_char_boundaries() {
        let s = "中文".repeat(100);
        let t = truncate_utf8(&s, 10);
        assert!(t.ends_with("…[truncated]"));
        assert!(t.starts_with('中'));
        assert_eq!(truncate_utf8("short", 100), "short");
    }

    #[test]
    fn full_access_contract_describes_root_mount() {
        let mount = crate::fs::Mount::from_canonical("/", PathBuf::from("/"), true).unwrap();
        let contract = contract_for(&[mount], "test-model");
        assert!(contract.contains("Writable mounts: /"), "{contract}");
    }
    #[test]
    fn mount_declarations_are_validated_or_rejected() {
        let mut ms = Vec::new();
        assert!(add_mount(&mut ms, "no-equals-sign").is_err());
        assert!(add_mount(&mut ms, "=path").is_err());
        assert!(add_mount(&mut ms, "rel/path=dir").is_err());
        assert!(add_mount(&mut ms, "/x=/definitely/not/here").is_err());
        assert!(ms.is_empty());
    }

    #[tokio::test]
    async fn async_function_body_and_explicit_return_work() {
        let (tx, _rx) = watch::channel(false);
        let out = eval_js(
            "function value() { return 41; }\nreturn {to: 'model', facts: {value: value() + 1}};",
            5_000,
            &[],
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(
            out.value,
            Some(serde_json::json!({"to": "model", "facts": {"value": 42}}))
        );
        assert_eq!(out.termination, Termination::Returned);
    }

    #[tokio::test]
    async fn returned_json_allows_realistic_results_but_remains_bounded() {
        let (tx, _rx) = watch::channel(false);
        let within_limit = eval_js("return 'x'.repeat(23 * 1024)", 5_000, &[], tx).await;
        assert!(within_limit.ok, "error: {:?}", within_limit.error);

        let (tx, _rx) = watch::channel(false);
        let oversized = eval_js("return 'x'.repeat(24 * 1024 + 1)", 5_000, &[], tx).await;
        assert!(!oversized.ok);
        let message = oversized.error.expect("result size error").message;
        assert!(message.contains("24576"), "{message}");
    }

    #[tokio::test]
    async fn invalid_timeout_is_configuration_failure() {
        let (tx, _rx) = watch::channel(false);
        let out = eval_js("return 1", 0, &[], tx).await;
        assert!(!out.ok);
        assert_eq!(out.termination, Termination::Fatal);
        assert_eq!(
            out.error.as_ref().map(|e| &e.kind),
            Some(&ErrorKind::Configuration)
        );
    }

    #[tokio::test]
    async fn stdout_cap_never_splits_a_codepoint() {
        let (tx, _rx) = watch::channel(false);
        let out = eval_js(
            "print(\"x\".repeat(16383) + \"中文中文中文\");\nreturn 1",
            5_000,
            &[],
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        assert!(out.stdout.starts_with("xxxx"));
        assert!(out.stdout.ends_with("…[truncated]"));
        assert!(out.stdout.len() <= 16 * 1024 + 32);
    }

    #[tokio::test]
    async fn print_storms_are_bounded_at_the_sink() {
        let (tx, _rx) = watch::channel(false);
        let out = eval_js("while (true) print('x'.repeat(4000))", 500, &[], tx).await;
        assert!(!out.ok && out.timed_out);
        assert!(out.stdout.len() <= 16 * 1024 + 32);
        assert!(out.stdout.ends_with("…[truncated]"));
    }
}
