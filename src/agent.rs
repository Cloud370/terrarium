//! Durable model-driven agent loop. The trusted outer loop owns the invocation filesystem
//! mode, operator write scopes, and the write-preauthorization lifecycle: every model
//! response's optional `access` block plus one `run` block is parsed here, resolved against
//! operator scopes, and — in `planned-write` — decided through an `Authorizer` before
//! QuickJS starts. The kernel only ever receives the frozen per-run authority.

use std::path::PathBuf;
use std::rc::Rc;

use tokio::sync::watch;

use crate::{
    auth::{
        covered_by_exec_grants, covered_by_scopes, freeze_authority, freeze_proc_authority,
        operator_exec_grant, parse_access_block, resolve_access_request, AccessBlock, Authorizer,
        Decision, DeclaredCommand, ResolvedAccessRequest,
    },
    config, eval_js,
    fs::{FilesystemMode, RunFilesystemAuthority, WriteScope},
    kernel::FACTS_CAP,
    llm,
    proc::{ProcAuthority, ProcTable},
    registry,
    session::{project, turn_data, Event, Journal},
    ErrorKind, Outcome, RunEnv, MAX_TIMEOUT_MS,
};

const COMMON: &str = include_str!("prompts/common.md");
const ROLE_TEMPLATE: &str = include_str!("prompts/main.md");
const RUN_TIMEOUT_DEFAULT_MS: u64 = 10_000;
const DEFAULT_MAX_STEPS: u64 = 256;
const FEEDBACK_CAP: usize = 24 * 1024;
/// One run journals at most this many capability receipts; later ones are dropped. A
/// handle-bearing receipt (a `spawn` anchor) is exempt: handles are bounded by the
/// process table, and dropping one would strand its `proc/exit` on the validator.
const RUN_RECEIPT_CAP: usize = 128;

/// The invocation-local facts rendered at the head of every newly emitted user-role message.
/// Deterministic by construction: same inputs, same bytes, so an unchanged state renders a
/// byte-identical block and a retried model request reuses exactly the same message bytes.
pub(crate) struct RuntimeState {
    working_root: String,
    mode: FilesystemMode,
    default_run_timeout_ms: u64,
    capabilities: String,
    platform: String,
    procs: Rc<ProcTable>,
}

impl RuntimeState {
    fn new(working_root: String, mode: FilesystemMode, default_run_timeout_ms: u64) -> Self {
        Self {
            working_root,
            mode,
            default_run_timeout_ms,
            capabilities: registry::capability_namespaces(),
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            procs: Rc::new(ProcTable::new(proc_log_root_for("stateless"))),
        }
    }

    fn with_procs(mut self, procs: Rc<ProcTable>) -> Self {
        self.procs = procs;
        self
    }

    fn block(&self) -> String {
        format!(
            "<terrarium-runtime-state>\n### Current runtime\n- Working root: `{}`\n- Platform: \
             `{}`\n- Filesystem mode: `{}`\n- Default run timeout: {} ms (hard cap {} ms)\n- Live \
             processes: `{}`\n- Installed host capabilities: `{}`\n</terrarium-runtime-state>",
            escape_state_text(&self.working_root),
            escape_state_text(&self.platform),
            self.mode.as_str(),
            self.default_run_timeout_ms,
            MAX_TIMEOUT_MS,
            escape_state_text(&self.procs.live_summary()),
            escape_state_text(&self.capabilities),
        )
    }

    /// `user.content = "<terrarium-runtime-state>…</terrarium-runtime-state>\n\n" + content`.
    fn prepend(&self, content: &str) -> String {
        format!("{}\n\n{}", self.block(), content)
    }
}

/// The spawn-log directory for one invocation: host-owned session state, never rendered
/// as a writable root.
fn proc_log_root_for(session_id: &str) -> PathBuf {
    config::state_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("terrarium/sessions"))
        .join(session_id)
        .join("procs")
}

/// The host owns every state value; escaping keeps a value from closing the wrapper.
fn escape_state_text(text: &str) -> String {
    text.replace('<', "&lt;").replace('>', "&gt;")
}

/// Everything one agent invocation freezes at launch: the filesystem mode, the
/// operator-declared write scopes and exec grants, the runtime-state renderer, the
/// process table, the network switch, and the authorizer the adapter supplied for user
/// decisions.
pub(crate) struct Invocation<'a> {
    mode: FilesystemMode,
    operator_scopes: Vec<WriteScope>,
    operator_execs: Vec<PathBuf>,
    offline: bool,
    working_root: PathBuf,
    table: Rc<ProcTable>,
    state: RuntimeState,
    authorizer: &'a dyn Authorizer,
}

impl Invocation<'_> {
    fn base_authority(&self) -> RunFilesystemAuthority {
        match self.mode {
            FilesystemMode::ReadOnly => RunFilesystemAuthority::ReadOnly,
            FilesystemMode::PlannedWrite => {
                RunFilesystemAuthority::Scoped(self.operator_scopes.clone())
            }
            FilesystemMode::FullAccess => RunFilesystemAuthority::FullAccess,
        }
    }

    fn base_proc_authority(&self) -> ProcAuthority {
        match self.mode {
            FilesystemMode::FullAccess => ProcAuthority::Unrestricted,
            _ => ProcAuthority::Denied,
        }
    }
}

pub struct RunProgram {
    pub code: String,
    pub timeout_ms: Option<u64>,
}

/// Only standalone `run` and `access` fence lines participate in the protocol.
fn scan_fences(reply: &str) -> (Vec<(&'static str, String)>, bool) {
    let mut blocks = Vec::new();
    let mut body: Option<(&'static str, String)> = None;
    for line in reply.lines() {
        let trimmed = line.trim();
        match (trimmed, body.is_some()) {
            ("```", true) => {
                let (kind, source) = body.take().expect("open body");
                blocks.push((kind, source.trim_start_matches('\n').to_string()));
            }
            ("```run", false) => body = Some(("run", String::new())),
            ("```access", false) => body = Some(("access", String::new())),
            _ => {
                if let Some((_, source)) = body.as_mut() {
                    source.push_str(line);
                    source.push('\n');
                }
            }
        }
    }
    (blocks, body.is_some())
}

fn parse_timeout_directive(code: &str) -> Option<u64> {
    let first = code.lines().find(|line| !line.trim().is_empty())?;
    first
        .trim()
        .strip_prefix("// timeout-ms:")?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|v| *v >= 1)
}

pub(crate) enum Extracted {
    Run {
        program: RunProgram,
        access: Option<AccessBlock>,
    },
    NoRun,
    Truncated,
    Multiple,
    AccessAfterRun,
    InvalidAccess(String),
}

/// One response is one closed `run` block, optionally preceded by one closed `access` block.
/// The parser is deliberately more forgiving than the instruction: an absent access block is
/// the empty request. Genuine ambiguity is a protocol error.
pub(crate) fn extract(reply: &str) -> Extracted {
    let (blocks, unclosed) = scan_fences(reply);
    if unclosed {
        return Extracted::Truncated;
    }
    let mut fences: Vec<(&'static str, usize)> = Vec::new();
    let mut run_body: Option<&String> = None;
    let mut access_body: Option<&String> = None;
    for (index, (kind, body)) in blocks.iter().enumerate() {
        fences.push((kind, index));
        match *kind {
            "run" => run_body = run_body.or(Some(body)),
            "access" => access_body = access_body.or(Some(body)),
            _ => {}
        }
    }
    let runs = fences.iter().filter(|(kind, _)| *kind == "run").count();
    let accesses = fences.iter().filter(|(kind, _)| *kind == "access").count();
    if runs == 0 {
        return Extracted::NoRun;
    }
    if runs > 1 || accesses > 1 {
        return Extracted::Multiple;
    }
    if let (Some((_, run_index)), Some((_, access_index))) = (
        fences.iter().find(|(kind, _)| *kind == "run"),
        fences.iter().find(|(kind, _)| *kind == "access"),
    ) {
        if access_index > run_index {
            return Extracted::AccessAfterRun;
        }
    }
    let access = match access_body {
        Some(body) => match parse_access_block(body.trim()) {
            Ok(block) => Some(block),
            Err(error) => return Extracted::InvalidAccess(error),
        },
        None => None,
    };
    let code = run_body.expect("at least one run body");
    Extracted::Run {
        program: RunProgram {
            timeout_ms: parse_timeout_directive(code),
            code: code.clone(),
        },
        access,
    }
}

fn write_feedback(out: &Outcome) -> Option<serde_json::Value> {
    if out.writes.is_empty() && !out.writes_truncated {
        return None;
    }
    Some(serde_json::json!({
        "writes": out.writes,
        "writesTruncated": out.writes_truncated,
    }))
}

fn feedback(out: &Outcome, turn: u64, step: u64) -> String {
    let mut run = serde_json::json!({
        "ok": out.ok,
        "error": out.error,
        "termination": out.termination,
        "timedOut": out.timed_out,
        "elapsedMs": out.elapsed_ms,
    });
    if let Some(writes) = write_feedback(out) {
        run["writes"] = writes["writes"].clone();
        run["writesTruncated"] = writes["writesTruncated"].clone();
    }
    let payload = serde_json::json!({
        "turn": turn,
        "step": step,
        "to": "model",
        "run": run,
    });

    if payload.to_string().len() <= FEEDBACK_CAP {
        return payload.to_string();
    }
    serde_json::json!({
        "turn": turn,
        "step": step,
        "to": "model",
        "run": {
            "ok": false,
            "error": {"kind": ErrorKind::Protocol, "message": "run result exceeded the feedback limit"},
            "termination": "failed",
            "timedOut": false,
            "elapsedMs": out.elapsed_ms,
        },
    })
    .to_string()
}

fn parse_disposition(value: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    let value = value.ok_or_else(|| {
        "agent program must end with a top-level return of {to: \"model\", facts: {...}} or {to: \"user\", message: \"...\"}; the run completed with no returned value — a return inside an async IIFE or callback never reaches the host".to_string()
    })?;
    let object = value.as_object().ok_or_else(|| {
        "agent program must return an object with to: \"model\" or to: \"user\"".to_string()
    })?;
    let to = object
        .get("to")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "agent program return is missing string field to".to_string())?;
    match to {
        "model" => {
            if object.len() != 2 || !object.contains_key("facts") {
                return Err("to: \"model\" requires exactly the fields to and facts".into());
            }
            if !object["facts"].is_object() {
                return Err("to: \"model\" requires facts to be an object".into());
            }
            if serde_json::to_vec(&object["facts"])
                .map(|facts| facts.len() > FACTS_CAP)
                .unwrap_or(true)
            {
                return Err(format!(
                    "to: \"model\" facts exceed the {FACTS_CAP}-byte limit; write large data to an authorized file and return its path"
                ));
            }
            Ok(serde_json::json!({"to":"model","facts":object["facts"]}))
        }
        "user" => {
            if object.len() != 2 || !object.contains_key("message") {
                return Err("to: \"user\" requires exactly the fields to and message".into());
            }
            let message = object["message"]
                .as_str()
                .ok_or_else(|| "to: \"user\" requires message to be a string".to_string())?;
            Ok(serde_json::json!({"to":"user","message":message}))
        }
        _ => Err(format!(
            "unsupported to value {to:?}; use \"model\" or \"user\""
        )),
    }
}

fn model_observation_with_writes(
    turn: u64,
    step: u64,
    disposition: &serde_json::Value,
    out: &Outcome,
) -> String {
    let mut result = serde_json::json!({
        "turn": turn,
        "step": step,
        "to": "model",
        "facts": disposition["facts"],
    });
    if let Some(writes) = write_feedback(out) {
        result["writes"] = writes["writes"].clone();
        result["writesTruncated"] = writes["writesTruncated"].clone();
    }
    result.to_string()
}

fn protocol_observation(turn: u64, step: u64, message: impl Into<String>) -> String {
    serde_json::json!({
        "turn": turn,
        "step": step,
        "to": "model",
        "error": {"kind": ErrorKind::Protocol, "message": message.into()},
    })
    .to_string()
}

fn protocol_observation_with_writes(
    turn: u64,
    step: u64,
    message: impl Into<String>,
    out: &Outcome,
) -> String {
    let mut result = serde_json::json!({
        "turn": turn,
        "step": step,
        "to": "model",
        "error": {"kind": ErrorKind::Protocol, "message": message.into()},
    });
    if let Some(writes) = write_feedback(out) {
        result["writes"] = writes["writes"].clone();
        result["writesTruncated"] = writes["writesTruncated"].clone();
    }
    result.to_string()
}

/// The structured preauthorization result the model receives when JavaScript did not start.
fn authorization_observation(
    turn: u64,
    step: u64,
    status: &str,
    request: &ResolvedAccessRequest,
    guidance: impl Into<String>,
) -> String {
    let mut authorization = serde_json::json!({
        "status": status,
        "writes": request.displays(),
        "reason": request.reason,
    });
    if !request.commands.is_empty() {
        authorization["commands"] = serde_json::json!(request.command_displays());
    }
    serde_json::json!({
        "turn": turn,
        "step": step,
        "to": "model",
        "authorization": authorization,
        "error": {"kind": ErrorKind::Protocol, "message": guidance.into()},
    })
    .to_string()
}

fn observation_for_extract(turn: u64, step: u64, extracted: &Extracted) -> Option<String> {
    Some(match extracted {
        Extracted::Truncated => protocol_observation(
            turn,
            step,
            "no program was executed; close the single ```run block (and the ```access block, when present) and send one complete program with no prose or other code block",
        ),
        Extracted::Multiple => protocol_observation(
            turn,
            step,
            "no program was executed; the response contained multiple run or access blocks; use exactly one optional ```access block followed by one complete ```run program",
        ),
        Extracted::NoRun => protocol_observation(
            turn,
            step,
            "no program was executed; send one optional ```access block followed by exactly one complete ```run program, with no prose or other code block",
        ),
        Extracted::AccessAfterRun => protocol_observation(
            turn,
            step,
            "no program was executed; the ```access block must precede the ```run block",
        ),
        Extracted::InvalidAccess(error) => protocol_observation(
            turn,
            step,
            format!("no program was executed; invalid access request: {error}"),
        ),
        Extracted::Run { .. } => return None,
    })
}

/// The byte-stable system prompt: role instructions, common principles, and the tool
/// contract. No mode, path, model, or timeout value is interpolated — those live in the
/// runtime-state block at the head of each user-role message.
fn system_prompt() -> String {
    format!(
        "{}\n\n{}\n\n{}",
        ROLE_TEMPLATE.trim_end(),
        COMMON,
        crate::contract()
    )
}

fn greeting_response(message: &str) -> Option<&'static str> {
    let normalized = message.trim().to_lowercase();
    let greeting = matches!(
        normalized.as_str(),
        "hi" | "hello"
            | "hello!"
            | "hey"
            | "hey!"
            | "你好"
            | "你好!"
            | "你好！"
            | "嗨"
            | "嗨!"
            | "嗨！"
    );
    greeting.then_some("你好！请告诉我需要处理的任务。")
}

fn start_turn(
    journal: &mut Journal,
    message: &str,
    profile: &config::ResolvedProfile,
    state: &RuntimeState,
    max_steps: u64,
    timeout: u64,
) -> Result<(), String> {
    let prompt = system_prompt();
    journal.append(
        "turn/start",
        turn_data(
            &state.prepend(message),
            &prompt,
            profile,
            max_steps,
            timeout,
            MAX_TIMEOUT_MS,
        ),
    )?;
    Ok(())
}

fn copy_turn(
    journal: &mut Journal,
    message: &str,
    previous: &Event,
    state: &RuntimeState,
) -> Result<(), String> {
    let mut data = previous.data.clone();
    data["message"] = serde_json::Value::String(state.prepend(message));
    data["systemPrompt"] = serde_json::Value::String(system_prompt());
    journal.append("turn/start", data)?;
    Ok(())
}

async fn execute_run(
    journal: &mut Journal,
    inv: &Invocation<'_>,
    run_seq: u64,
    at: (u64, u64),
    action: &serde_json::Value,
    authorities: (&RunFilesystemAuthority, &ProcAuthority),
    limits: (u64, u64),
) -> Result<(), String> {
    let (turn, step) = at;
    let (authority, proc_authority) = authorities;
    let state = &inv.state;
    let source = action["source"]
        .as_str()
        .ok_or_else(|| "run action has no source".to_string())?;
    let timeout = action["timeoutMs"]
        .as_u64()
        .unwrap_or(limits.0)
        .min(limits.1)
        .min(MAX_TIMEOUT_MS);
    let receipts = RunEnv::receipts();
    let env = RunEnv {
        fs: authority.clone(),
        proc: proc_authority.clone(),
        net_offline: inv.offline,
        table: inv.table.clone(),
        working_root: inv.working_root.clone(),
        receipts: receipts.clone(),
    };
    let (cancel_tx, _cancel_rx) = watch::channel(false);
    let outcome = eval_js(source, timeout, &env, cancel_tx).await;
    journal_receipts(journal, Some(run_seq), &outcome.receipts)?;
    let pending = inv.table.take_exit_receipts();
    journal_exit_receipts(journal, &pending)?;
    let returned = outcome.value.clone();
    let mut data = serde_json::json!({
        "runSeq": run_seq,
        "status": "completed",
        "outcome": {
            "ok": outcome.ok,
            "value": outcome.value,
            "stdout": outcome.stdout,
            "error": outcome.error,
            "termination": outcome.termination,
            "timedOut": outcome.timed_out,
            "elapsedMs": outcome.elapsed_ms,
            "writes": outcome.writes,
            "writesTruncated": outcome.writes_truncated,
        },
    });
    if outcome.ok {
        match parse_disposition(returned) {
            Ok(disposition) if disposition["to"] == "model" => {
                data["disposition"] = disposition.clone();
                data["observation"] = serde_json::Value::String(state.prepend(
                    &model_observation_with_writes(turn, step, &disposition, &outcome),
                ));
            }
            Ok(disposition) => {
                data["disposition"] = disposition;
            }
            Err(error) => {
                data["observation"] = serde_json::Value::String(state.prepend(
                    &protocol_observation_with_writes(turn, step, error, &outcome),
                ));
            }
        }
    } else {
        data["observation"] =
            serde_json::Value::String(state.prepend(&feedback(&outcome, turn, step)));
    }
    journal.append("run/result", data)?;
    Ok(())
}

/// Stamp and append the receipts a run collected: `run/spawn` and `net/request` carry the
/// runSeq; `proc/exit` records are run-independent. The journal never stores stream data
/// beyond the bounded receipt tails. Truncation is never silent: a `receipts/truncated`
/// event counts what was dropped, because the journal is the audit trail (and the only
/// detection mechanism for `net/request` egress). A handle-bearing receipt is never
/// capped: it is the anchor a later `proc/exit` resolves its handle against, and handles
/// are bounded by the process table, so dropping one would strand its exit receipt on the
/// validator.
fn journal_receipts(
    journal: &mut Journal,
    run_seq: Option<u64>,
    receipts: &[serde_json::Value],
) -> Result<(), String> {
    let mut journaled = 0;
    let mut dropped = 0;
    for receipt in receipts {
        let (kind, mut data) = match (
            receipt.get("handle"),
            receipt.get("status"),
            receipt.get("exe"),
        ) {
            (_, Some(_), _) => ("net/request", receipt.clone()),
            (Some(_), _, _) => ("run/spawn", receipt.clone()),
            (None, _, Some(_)) => ("run/spawn", receipt.clone()),
            (None, _, None) => ("proc/exit", receipt.clone()),
        };
        if let Some(run_seq) = run_seq {
            if matches!(kind, "run/spawn" | "net/request") {
                data["runSeq"] = serde_json::json!(run_seq);
                // keep field order deterministic in the journal line
                let mut ordered = serde_json::Map::new();
                ordered.insert("runSeq".into(), data["runSeq"].clone());
                if let Some(fields) = data.as_object() {
                    for (key, value) in fields {
                        if key != "runSeq" {
                            ordered.insert(key.clone(), value.clone());
                        }
                    }
                }
                data = serde_json::Value::Object(ordered);
            }
        }
        let anchored = data.get("handle").is_some();
        if !anchored {
            if journaled >= RUN_RECEIPT_CAP {
                dropped += 1;
                continue;
            }
            journaled += 1;
        }
        journal.append(kind, data)?;
    }
    if dropped > 0 {
        let mut data = serde_json::json!({"dropped": dropped});
        if let Some(run_seq) = run_seq {
            data["runSeq"] = serde_json::json!(run_seq);
        }
        journal.append("receipts/truncated", data)?;
    }
    Ok(())
}

/// Append `proc/exit` receipts observed outside any run (a process that died while the
/// model was thinking, or at session shutdown).
fn journal_exit_receipts(
    journal: &mut Journal,
    receipts: &[serde_json::Value],
) -> Result<(), String> {
    let dropped = receipts.len().saturating_sub(RUN_RECEIPT_CAP);
    for receipt in receipts.iter().take(RUN_RECEIPT_CAP) {
        journal.append("proc/exit", receipt.clone())?;
    }
    if dropped > 0 {
        journal.append(
            "receipts/truncated",
            serde_json::json!({"dropped": dropped}),
        )?;
    }
    Ok(())
}

/// One preauthorization decision: the frozen authorities for the run (or none when
/// JavaScript must not start) plus the bounded `run/access` journal event.
struct AccessDecision {
    authority: Option<RunFilesystemAuthority>,
    proc_authority: Option<ProcAuthority>,
    event: serde_json::Value,
}

fn access_block_of(action: &serde_json::Value) -> AccessBlock {
    let writes = action["access"]["writes"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let commands = action["access"]["commands"]
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let record = entry.as_object()?;
                    Some(DeclaredCommand {
                        exe: record.get("exe")?.as_str()?.to_string(),
                        argv: record
                            .get("argv")?
                            .as_array()?
                            .iter()
                            .filter_map(|arg| arg.as_str().map(str::to_string))
                            .collect(),
                        cwd: record
                            .get("cwd")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let reason = action["access"]["reason"].as_str().unwrap_or_default();
    AccessBlock {
        writes,
        commands,
        reason: reason.to_string(),
    }
}

/// The complete planned-write lifecycle plus the mode-specific meaning of a declaration:
/// resolve → subtract operator scopes and exec grants → decide → freeze. Writes and
/// commands are one request and one decision. An empty request runs directly under the
/// invocation's base authority and journals nothing.
fn authorize_run_access(
    inv: &Invocation,
    action: &serde_json::Value,
    turn: u64,
    step: u64,
) -> AccessDecision {
    let block = access_block_of(action);
    if block.writes.is_empty() && block.commands.is_empty() {
        return AccessDecision {
            authority: Some(inv.base_authority()),
            proc_authority: Some(inv.base_proc_authority()),
            event: serde_json::Value::Null,
        };
    }
    let access_event = |decision: &str,
                        request: &ResolvedAccessRequest,
                        observation: Option<String>|
     -> serde_json::Value {
        let mut event = serde_json::json!({
            "decision": decision,
            "writes": request.displays(),
            "reason": request.reason,
        });
        if !request.commands.is_empty() {
            event["commands"] = serde_json::json!(request.command_displays());
        }
        if let Some(text) = observation {
            event["observation"] = serde_json::Value::String(text);
        }
        event
    };
    let resolved = match resolve_access_request(&block, inv.mode, &inv.working_root) {
        Ok(resolved) => resolved,
        Err(error) => {
            let request = ResolvedAccessRequest {
                targets: Vec::new(),
                commands: Vec::new(),
                reason: block.reason.clone(),
            };
            let observation = authorization_observation(
                turn,
                step,
                "authorization_invalid",
                &request,
                format!(
                    "no program was executed; the access request is invalid: {error}. Fix the \
                     access block and resend one complete program"
                ),
            );
            return AccessDecision {
                authority: None,
                proc_authority: None,
                event: access_event("invalid", &request, Some(observation)),
            };
        }
    };
    match inv.mode {
        FilesystemMode::FullAccess => AccessDecision {
            authority: Some(RunFilesystemAuthority::FullAccess),
            proc_authority: Some(ProcAuthority::Unrestricted),
            event: access_event("declared", &resolved, None),
        },
        FilesystemMode::ReadOnly => {
            let observation = authorization_observation(
                turn,
                step,
                "authorization_denied",
                &resolved,
                "no program was executed; the current invocation is read-only and every write \
                 and every process launch is denied. Do not request writes or commands again in \
                 this invocation; continue with read-only work, host.net.fetch, or hand off to \
                 the user.",
            );
            AccessDecision {
                authority: None,
                proc_authority: None,
                event: access_event("deny", &resolved, Some(observation)),
            }
        }
        FilesystemMode::PlannedWrite => {
            let remainder_writes: Vec<_> = resolved
                .targets
                .iter()
                .filter(|target| !covered_by_scopes(&inv.operator_scopes, target))
                .cloned()
                .collect();
            let remainder_commands: Vec<_> = resolved
                .commands
                .iter()
                .filter(|command| !covered_by_exec_grants(&inv.operator_execs, command))
                .cloned()
                .collect();
            if remainder_writes.is_empty() && remainder_commands.is_empty() {
                return AccessDecision {
                    authority: Some(freeze_authority(&inv.operator_scopes, &[])),
                    proc_authority: Some(freeze_proc_authority(&inv.operator_execs, &[])),
                    event: access_event("covered", &resolved, None),
                };
            }
            let prompt_request = ResolvedAccessRequest {
                targets: remainder_writes,
                commands: remainder_commands,
                reason: resolved.reason.clone(),
            };
            match inv.authorizer.decide(&prompt_request) {
                Decision::Allow => AccessDecision {
                    authority: Some(freeze_authority(
                        &inv.operator_scopes,
                        &prompt_request.targets,
                    )),
                    proc_authority: Some(freeze_proc_authority(
                        &inv.operator_execs,
                        &prompt_request.commands,
                    )),
                    event: access_event("allow", &prompt_request, None),
                },
                Decision::Deny => AccessDecision {
                    authority: None,
                    proc_authority: None,
                    event: access_event(
                        "deny",
                        &prompt_request,
                        Some(authorization_observation(
                            turn,
                            step,
                            "authorization_denied",
                            &prompt_request,
                            "no program was executed; the user denied the requested write and \
                             command set. Do not re-request the same set within this turn; \
                             continue read-only or hand off to the user",
                        )),
                    ),
                },
                Decision::Cancel => AccessDecision {
                    authority: None,
                    proc_authority: None,
                    event: access_event(
                        "cancel",
                        &prompt_request,
                        Some(authorization_observation(
                            turn,
                            step,
                            "authorization_cancelled",
                            &prompt_request,
                            "no program was executed; the user cancelled the authorization \
                             request. Do not re-request the same set within this turn; continue \
                             read-only or hand off to the user",
                        )),
                    ),
                },
                Decision::Unavailable => AccessDecision {
                    authority: None,
                    proc_authority: None,
                    event: access_event(
                        "unavailable",
                        &prompt_request,
                        Some(authorization_observation(
                            turn,
                            step,
                            "authorization_unavailable",
                            &prompt_request,
                            "no program was executed; no interactive authorizer is available in \
                             this invocation, so no write or command can be authorized here. \
                             Continue read-only or hand off to the user",
                        )),
                    ),
                },
            }
        }
    }
}

fn recover_unknown_run(journal: &mut Journal, run_seq: u64) -> Result<(), String> {
    journal.append(
        "run/result",
        serde_json::json!({
            "runSeq": run_seq,
            "status": "outcome_unknown",
            "observation": "the previous program may have changed state before the process stopped; it was not repeated, so inspect current state before proceeding"
        }),
    )?;
    Ok(())
}

fn profile_from_turn(turn: &Event) -> config::ResolvedProfile {
    serde_json::from_value(turn.data["profile"].clone()).expect("validated turn profile")
}

fn turn_limit(turn: &Event) -> u64 {
    turn.data["limits"]["maxSteps"]
        .as_u64()
        .unwrap_or(DEFAULT_MAX_STEPS)
}

fn next_step(journal: &Journal) -> u64 {
    let turn_seq = journal
        .events
        .iter()
        .rfind(|event| event.kind == "turn/start")
        .map(|event| event.seq)
        .unwrap_or(0);
    journal
        .events
        .iter()
        .filter(|event| event.kind == "model/request" && event.seq > turn_seq)
        .filter_map(|event| event.data["step"].as_u64())
        .max()
        .unwrap_or(0)
        + 1
}

fn process_model_result(
    journal: &mut Journal,
    request_seq: u64,
    turn: u64,
    step: u64,
    reply: llm::ModelReply,
    limits: (u64, u64),
    state: &RuntimeState,
) -> Result<(), String> {
    let extracted = extract(&reply.content);
    let action = if let Some(message) = observation_for_extract(turn, step, &extracted) {
        serde_json::json!({"kind":"observation","message":state.prepend(&message)})
    } else if let Extracted::Run { program, access } = extracted {
        let mut access_json = serde_json::json!({
            "writes": access.as_ref().map(|b| b.writes.clone()).unwrap_or_default(),
            "reason": access.as_ref().map(|b| b.reason.clone()).unwrap_or_default(),
        });
        let commands = access
            .as_ref()
            .map(|b| b.commands.clone())
            .unwrap_or_default();
        if !commands.is_empty() {
            // the journal keeps the exact declared records; display truncation happens
            // only in prompts and run/access events
            access_json["commands"] = serde_json::to_value(&commands).unwrap_or_default();
        }
        serde_json::json!({
            "kind": "run",
            "source": program.code,
            "timeoutMs": program.timeout_ms.unwrap_or(limits.0).min(limits.1),
            "access": access_json,
        })
    } else {
        unreachable!()
    };
    let mut data = serde_json::json!({
        "requestSeq": request_seq,
        "ok": true,
        "content": reply.content,
        "action": action,
    });
    data["usage"] = serde_json::to_value(reply.usage).expect("usage serializes");
    if let Some(value) = reply.reasoning.as_ref().and_then(reasoning_json) {
        data["reasoning"] = value;
    }
    journal.append("model/result", data)?;
    Ok(())
}

/// Journals reasoning within the schema caps. A blob whose text exceeds the
/// cap is dropped whole rather than truncated: a mid-character byte cut would
/// panic, a truncation marker would push the text past the journal's own
/// limit, and a shortened Anthropic thinking text would no longer match its
/// stored signature. An oversized replay payload alone is nulled — the text
/// stays for forensics and the next turn simply replays nothing.
fn reasoning_json(reasoning: &llm::ReasoningBlob) -> Option<serde_json::Value> {
    if reasoning.text.len() > llm::REASONING_TEXT_CAP {
        return None;
    }
    let replay = match serde_json::to_vec(&reasoning.replay) {
        Ok(bytes) if bytes.len() <= llm::REASONING_REPLAY_CAP => reasoning.replay.clone(),
        _ => serde_json::Value::Null,
    };
    Some(serde_json::json!({ "text": reasoning.text, "replay": replay }))
}

/// Streams deltas to stderr as a live operator preview. Thinking arrives
/// dimmed so it reads as background; text is passed through raw.
fn delta_preview(event: llm::DeltaEvent<'_>) {
    use std::io::Write;
    let mut stderr = std::io::stderr();
    match event {
        llm::DeltaEvent::Thinking(text) => {
            let _ = write!(stderr, "\x1b[2m{text}\x1b[0m");
        }
        llm::DeltaEvent::Text(text) => {
            let _ = write!(stderr, "{text}");
        }
    }
    let _ = stderr.flush();
}

/// One operator-facing line per model call: token accounting plus the
/// projected context footprint against the profile's declared window.
fn report_usage(step: u64, usage: &llm::Usage, profile: &config::ResolvedProfile) {
    let context = usage.context_tokens();
    let budget = match profile.context_window {
        Some(window) if window > 0 => {
            let pct = (context as f64 / window as f64 * 100.0).round() as u64;
            let flag = if pct >= 85 { " ⚠ near limit" } else { "" };
            format!(" · context {context}/{window} tok ({pct}%){flag}")
        }
        _ => format!(" · context {context} tok"),
    };
    eprintln!(
        "⟡ step {step} · in {} tok (cache read {}, write {}) · out {} tok{}",
        usage.input_tokens,
        usage.cache_read_tokens,
        usage.cache_write_tokens,
        usage.output_tokens,
        budget
    );
}

fn turn_timeouts(turn: &Event) -> (u64, u64) {
    (
        turn.data["limits"]["defaultRunTimeoutMs"]
            .as_u64()
            .unwrap_or(RUN_TIMEOUT_DEFAULT_MS),
        turn.data["limits"]["maxRunTimeoutMs"]
            .as_u64()
            .unwrap_or(MAX_TIMEOUT_MS),
    )
}

async fn model_attempt(
    journal: &mut Journal,
    turn: &Event,
    step: u64,
    attempt: u64,
    state: &RuntimeState,
) -> Result<(), String> {
    let request_seq = journal.append(
        "model/request",
        serde_json::json!({"step":step,"attempt":attempt}),
    )?;
    let messages = project(&journal.events, request_seq);
    let profile = profile_from_turn(turn);
    match llm::stream(&profile, messages, &mut delta_preview).await {
        Ok(reply) => {
            eprintln!();
            report_usage(step, &reply.usage, &profile);
            let limits = turn_timeouts(turn);
            let turn_number = journal
                .events
                .iter()
                .filter(|event| event.kind == "turn/start" && event.seq <= turn.seq)
                .count() as u64;
            process_model_result(
                journal,
                request_seq,
                turn_number,
                step,
                reply,
                limits,
                state,
            )
        }
        Err(error) => {
            journal.append(
                "model/result",
                transport_model_result(request_seq, attempt, &error),
            )?;
            Ok(())
        }
    }
}

fn transport_model_result(
    request_seq: u64,
    attempt: u64,
    error: &llm::LlmError,
) -> serde_json::Value {
    serde_json::json!({
        "requestSeq": request_seq,
        "ok": false,
        "error": {
            "kind": error.kind,
            "message": error.message,
            "retryable": error.retryable && attempt == 1,
        }
    })
}

fn interrupted_model_result(request: &Event) -> serde_json::Value {
    serde_json::json!({
        "requestSeq": request.seq,
        "ok": false,
        "error": {
            "kind": "interrupted",
            "message": "request interrupted before result was durable",
            "retryable": request.data["attempt"].as_u64() == Some(1),
        }
    })
}

/// Rebuild the frozen authorities for a run whose decision was already journaled — the
/// crash-resume path between a durable `run/access` and its missing `run/start`. Historical
/// decisions are not authority by themselves: the reconstructed set is re-resolved from the
/// journaled request against the current invocation's mode, operator scopes, and exec
/// grants. A journaled decision only carries when the resumed invocation runs the mode that
/// produced it — a read-only resume never executes an approved planned-write run, and a
/// full-access resume does not inherit a scoped decision; a mode change drops the pending
/// run to a fresh model step.
fn authority_from_journaled_decision(
    inv: &Invocation,
    action: &serde_json::Value,
    decision: &str,
) -> Option<(RunFilesystemAuthority, ProcAuthority)> {
    let resolved = resolve_access_request(&access_block_of(action), inv.mode, &inv.working_root);
    match (inv.mode, decision) {
        (FilesystemMode::FullAccess, "declared") => Some((
            RunFilesystemAuthority::FullAccess,
            ProcAuthority::Unrestricted,
        )),
        (FilesystemMode::PlannedWrite, "covered") => Some((
            freeze_authority(&inv.operator_scopes, &[]),
            freeze_proc_authority(&inv.operator_execs, &[]),
        )),
        (FilesystemMode::PlannedWrite, "allow") => resolved.ok().map(|resolved| {
            (
                freeze_authority(&inv.operator_scopes, &resolved.targets),
                freeze_proc_authority(&inv.operator_execs, &resolved.commands),
            )
        }),
        _ => None,
    }
}

/// Append `run/start` and execute the program under the frozen authorities. Shared by the
/// fresh-decision path and the crash-resume path; the journal keeps them indistinguishable.
async fn start_and_execute_run(
    journal: &mut Journal,
    turn: &Event,
    inv: &Invocation<'_>,
    result: &Event,
    authority: RunFilesystemAuthority,
    proc_authority: ProcAuthority,
) -> Result<(), String> {
    let run_seq = journal.append(
        "run/start",
        serde_json::json!({"modelResultSeq": result.seq}),
    )?;
    let limits = turn_timeouts(turn);
    let turn_number = journal
        .events
        .iter()
        .filter(|event| event.kind == "turn/start" && event.seq <= turn.seq)
        .count() as u64;
    let step = journal
        .events
        .iter()
        .find(|event| event.seq == result.data["requestSeq"].as_u64().unwrap_or(0))
        .and_then(|event| event.data["step"].as_u64())
        .unwrap_or(1);
    execute_run(
        journal,
        inv,
        run_seq,
        (turn_number, step),
        &result.data["action"],
        (&authority, &proc_authority),
        limits,
    )
    .await
}

async fn drive(journal: Journal, inv: &Invocation<'_>) -> i32 {
    let mut journal = journal;
    loop {
        let Some(turn) = journal.open_turn().cloned() else {
            return 1;
        };
        let Some(last) = journal.events.last().cloned() else {
            return 1;
        };
        // processes that died while no run was active still owe their exit receipts
        if let Err(e) = journal_exit_receipts(&mut journal, &inv.table.take_exit_receipts()) {
            eprintln!("terrarium: {e}");
            return 1;
        }
        match last.kind.as_str() {
            "model/request" => {
                if journal.events.iter().any(|event| {
                    event.kind == "model/result"
                        && event.data["requestSeq"].as_u64() == Some(last.seq)
                }) {
                    continue;
                }
                if journal
                    .append("model/result", interrupted_model_result(&last))
                    .is_err()
                {
                    return 1;
                }
                continue;
            }
            "model/result" if last.data["ok"].as_bool() == Some(false) => {
                let request_seq = last.data["requestSeq"].as_u64().unwrap_or(0);
                let request = journal.events.iter().find(|event| event.seq == request_seq);
                let attempt = request
                    .and_then(|event| event.data["attempt"].as_u64())
                    .unwrap_or(2);
                if attempt == 1 && last.data["error"]["retryable"].as_bool() == Some(true) {
                    let step = request
                        .and_then(|event| event.data["step"].as_u64())
                        .unwrap_or(1);
                    if let Err(e) = model_attempt(&mut journal, &turn, step, 2, &inv.state).await {
                        eprintln!("terrarium: {e}");
                        return 1;
                    }
                    continue;
                }
                let reason = if last.data["error"]["kind"] == "cancelled" {
                    "cancelled"
                } else {
                    "failed"
                };
                if reason == "failed" {
                    if let Some(message) = last.data["error"]["message"].as_str() {
                        eprintln!("terrarium: model call failed after retries: {message}");
                    }
                }
                if journal
                    .append("turn/end", serde_json::json!({"reason":reason}))
                    .is_err()
                {
                    return 1;
                }
                continue;
            }
            "model/result"
                if last.data["ok"].as_bool() == Some(true)
                    && last.data["action"]["kind"] == "run" =>
            {
                // the model result is the last event, so no run/access or run/start for it
                // exists yet: decide now, journal the decision, then start the run
                let turn_number = journal
                    .events
                    .iter()
                    .filter(|event| event.kind == "turn/start" && event.seq <= turn.seq)
                    .count() as u64;
                let step = journal
                    .events
                    .iter()
                    .find(|event| event.seq == last.data["requestSeq"].as_u64().unwrap_or(0))
                    .and_then(|event| event.data["step"].as_u64())
                    .unwrap_or(1);
                let decision = authorize_run_access(inv, &last.data["action"], turn_number, step);
                let mut event = decision.event;
                if !event.is_null() {
                    event["modelResultSeq"] = serde_json::json!(last.seq);
                    if event.get("observation").and_then(|o| o.as_str()).is_some() {
                        let observation = event["observation"]
                            .as_str()
                            .expect("observation string")
                            .to_string();
                        event["observation"] =
                            serde_json::Value::String(inv.state.prepend(&observation));
                    }
                    if journal.append("run/access", event).is_err() {
                        return 1;
                    }
                }
                let (Some(authority), Some(proc_authority)) =
                    (decision.authority, decision.proc_authority)
                else {
                    continue;
                };
                if start_and_execute_run(&mut journal, &turn, inv, &last, authority, proc_authority)
                    .await
                    .is_err()
                {
                    return 1;
                }
                continue;
            }
            // A durable decision is the resume point for a crash between approval and
            // run/start. Blocking decisions already carry their observation — the next
            // model step proceeds from it. A permissive decision rebuilds the frozen
            // authority and starts the run here: its first and only execution, because no
            // run/start exists. Paths that no longer resolve, and decisions journaled under
            // a mode other than the current invocation's, fall through to the next model
            // step, whose fresh decision surfaces the changed state.
            "run/access" => {
                let blocking = matches!(
                    last.data["decision"].as_str(),
                    Some("deny" | "cancel" | "unavailable" | "invalid")
                );
                let result_seq = last.data["modelResultSeq"].as_u64();
                if !blocking {
                    if let Some(result) = journal
                        .events
                        .iter()
                        .find(|event| Some(event.seq) == result_seq && event.kind == "model/result")
                        .cloned()
                    {
                        if let Some((authority, proc_authority)) = authority_from_journaled_decision(
                            inv,
                            &result.data["action"],
                            last.data["decision"].as_str().unwrap_or_default(),
                        ) {
                            if start_and_execute_run(
                                &mut journal,
                                &turn,
                                inv,
                                &result,
                                authority,
                                proc_authority,
                            )
                            .await
                            .is_err()
                            {
                                return 1;
                            }
                            continue;
                        }
                    }
                }
            }
            "run/start" => {
                if !journal.events.iter().any(|event| {
                    event.kind == "run/result" && event.data["runSeq"].as_u64() == Some(last.seq)
                }) && recover_unknown_run(&mut journal, last.seq).is_err()
                {
                    return 1;
                }
                continue;
            }
            "run/result" => {
                if last.data["status"] == "completed" {
                    if last.data["disposition"]["to"] == "user" {
                        let message = last.data["disposition"]["message"]
                            .as_str()
                            .unwrap_or_default();
                        // the session ends normally here: kill every live process and
                        // journal the exit receipts while the turn is still open
                        let exits = inv.table.shutdown().await;
                        if let Err(e) = journal_exit_receipts(&mut journal, &exits) {
                            eprintln!("terrarium: {e}");
                            return 1;
                        }
                        if journal
                            .append(
                                "turn/end",
                                serde_json::json!({
                                    "reason":"handed_off",
                                    "handoffRunSeq":last.data["runSeq"]
                                }),
                            )
                            .is_err()
                        {
                            return 1;
                        }
                        println!("{message}");
                        return 0;
                    }
                    // Read old journals written before tagged dispositions existed.
                    if last.data.get("disposition").is_none() {
                        if let Some(answer) = last.data["outcome"]["answer"].as_str() {
                            if journal
                                .append(
                                    "turn/end",
                                    serde_json::json!({
                                        "reason":"answered",
                                        "answerRunSeq":last.data["runSeq"]
                                    }),
                                )
                                .is_err()
                            {
                                return 1;
                            }
                            println!("{answer}");
                            return 0;
                        }
                    }
                }
            }
            "turn/start" => {}
            _ => {}
        }

        let step = next_step(&journal);
        if step > turn_limit(&turn) {
            if journal
                .append("turn/end", serde_json::json!({"reason":"step_limit"}))
                .is_err()
            {
                return 1;
            }
            continue;
        }
        if let Err(e) = model_attempt(&mut journal, &turn, step, 1, &inv.state).await {
            eprintln!("terrarium: {e}");
            return 1;
        }
    }
}

fn read_message(args: &[String]) -> Result<String, String> {
    if args.is_empty() {
        return std::io::read_to_string(std::io::stdin())
            .map_err(|e| format!("stdin is not valid UTF-8: {e}"));
    }
    Ok(args.join(" "))
}

/// Flag-conflict check plus operator-scope resolution: everything about an invocation that
/// can fail. Runs before any durable state exists, so a rejected launch leaves no session
/// behind.
/// Everything the launch flags decide before any session exists: the filesystem mode,
/// the operator write scopes, the operator exec grants, and the offline switch.
pub(crate) struct LaunchOptions {
    mode: FilesystemMode,
    scopes: Vec<WriteScope>,
    execs: Vec<PathBuf>,
    offline: bool,
}

fn resolve_invocation(
    read_only: bool,
    full_access: bool,
    allow_write: &[String],
    allow_exec: &[String],
    offline: bool,
) -> Result<LaunchOptions, String> {
    if read_only && full_access {
        return Err("--read-only and --full-access are mutually exclusive".into());
    }
    if (read_only || full_access) && !allow_write.is_empty() {
        return Err(
            "--allow-write is valid only in planned-write mode; it cannot be combined with \
             --read-only or --full-access"
                .into(),
        );
    }
    if (read_only || full_access) && !allow_exec.is_empty() {
        return Err(
            "--allow-exec is valid only in planned-write mode; it cannot be combined with \
             --read-only or --full-access"
                .into(),
        );
    }
    let mode = if read_only {
        FilesystemMode::ReadOnly
    } else if full_access {
        FilesystemMode::FullAccess
    } else {
        FilesystemMode::PlannedWrite
    };
    let mut operator_scopes = Vec::with_capacity(allow_write.len());
    for spec in allow_write {
        operator_scopes.push(WriteScope::from_operator_spec(spec)?);
    }
    let mut execs = Vec::with_capacity(allow_exec.len());
    for name in allow_exec {
        execs.push(operator_exec_grant(name)?);
    }
    Ok(LaunchOptions {
        mode,
        scopes: operator_scopes,
        execs,
        offline,
    })
}

fn invocation<'a>(
    launch: LaunchOptions,
    working_root: String,
    canonical_root: PathBuf,
    session_id: &str,
    timeout: u64,
    authorizer: &'a dyn Authorizer,
) -> Invocation<'a> {
    let table = Rc::new(ProcTable::new(proc_log_root_for(session_id)));
    Invocation {
        mode: launch.mode,
        operator_scopes: launch.scopes,
        operator_execs: launch.execs,
        offline: launch.offline,
        working_root: canonical_root,
        table: table.clone(),
        state: RuntimeState::new(working_root, launch.mode, timeout).with_procs(table),
        authorizer,
    }
}

pub async fn run_cli(args: &[String], authorizer: &dyn Authorizer) -> i32 {
    let mut profile: Option<String> = None;
    let mut config_path: Option<std::path::PathBuf> = None;
    let mut resume: Option<String> = None;
    let mut read_only = false;
    let mut full = false;
    let mut allow_write: Vec<String> = Vec::new();
    let mut allow_exec: Vec<String> = Vec::new();
    let mut offline = false;
    let mut max_steps = DEFAULT_MAX_STEPS;
    let mut timeout = RUN_TIMEOUT_DEFAULT_MS;
    let mut message = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" if i + 1 < args.len() => {
                profile = Some(args[i + 1].clone());
                i += 2;
            }
            "--config" if i + 1 < args.len() => {
                config_path = Some(args[i + 1].clone().into());
                i += 2;
            }
            "--resume" if i + 1 < args.len() => {
                resume = Some(args[i + 1].clone());
                i += 2;
            }
            "--read-only" => {
                read_only = true;
                i += 1;
            }
            "--full-access" => {
                full = true;
                i += 1;
            }
            "--allow-write" if i + 1 < args.len() => {
                allow_write.push(args[i + 1].clone());
                i += 2;
            }
            "--allow-write" => {
                eprintln!("terrarium: --allow-write expects an absolute DIR or FILE path");
                return 2;
            }
            "--allow-exec" if i + 1 < args.len() => {
                allow_exec.push(args[i + 1].clone());
                i += 2;
            }
            "--allow-exec" => {
                eprintln!("terrarium: --allow-exec expects an executable NAME or absolute path");
                return 2;
            }
            "--offline" => {
                offline = true;
                i += 1;
            }
            "--max-steps" if i + 1 < args.len() => {
                max_steps = args[i + 1].parse().ok().filter(|v| *v >= 1).unwrap_or(0);
                if max_steps == 0 {
                    eprintln!("terrarium: --max-steps expects an integer >= 1");
                    return 2;
                }
                i += 2;
            }
            "--run-timeout-ms" if i + 1 < args.len() => {
                timeout = args[i + 1]
                    .parse()
                    .ok()
                    .filter(|v| (1..=MAX_TIMEOUT_MS).contains(v))
                    .unwrap_or(0);
                if timeout == 0 {
                    eprintln!("terrarium: --run-timeout-ms expects 1..={MAX_TIMEOUT_MS}");
                    return 2;
                }
                i += 2;
            }
            arg if arg.starts_with("--") => {
                eprintln!("terrarium: unknown or incomplete flag: {arg}");
                return 2;
            }
            value => {
                message.push(value.to_string());
                i += 1;
            }
        }
    }
    let is_resume = resume.is_some();
    let text = if is_resume {
        message.join(" ")
    } else if message.is_empty() {
        match read_message(&message) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("terrarium: {e}");
                return 2;
            }
        }
    } else {
        message.join(" ")
    };
    if !is_resume && text.trim().is_empty() {
        eprintln!("terrarium: a message is required");
        return 2;
    }
    if !is_resume {
        if let Some(answer) = greeting_response(&text) {
            println!("{answer}");
            return 0;
        }
    }
    // validate the launch before touching durable state: a bad flag combination or an
    // unresolvable --allow-write / --allow-exec grant must not leave a fresh session behind
    let launch = match resolve_invocation(read_only, full, &allow_write, &allow_exec, offline) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("terrarium: {e}");
            return 2;
        }
    };
    let mut journal = if let Some(id) = &resume {
        match Journal::open(id) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("terrarium: {e}");
                return 2;
            }
        }
    } else {
        let root = match std::env::current_dir() {
            Ok(root) => root,
            Err(error) => {
                eprintln!("terrarium: cannot determine working root: {error}");
                return 2;
            }
        };
        match Journal::create(&root) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("terrarium: {e}");
                return 2;
            }
        }
    };
    let root = journal.header.working_root.display_path.clone();
    let canonical_root = PathBuf::from(journal.header.working_root.canonical_path.clone());
    let session_id = journal.id.clone();
    let inv = invocation(
        launch,
        root,
        canonical_root,
        &session_id,
        timeout,
        authorizer,
    );
    if journal.open_turn().is_some() {
        if !message.is_empty() || profile.is_some() || config_path.is_some() {
            eprintln!(
                "terrarium: an open turn can only be resumed without a new profile or message"
            );
            return 2;
        }
    } else {
        let text = text.clone();
        if text.trim().is_empty() {
            eprintln!("terrarium: a message is required");
            return 2;
        }
        let previous = journal
            .events
            .iter()
            .rfind(|event| event.kind == "turn/start")
            .cloned();
        if is_resume && profile.is_none() && config_path.is_some() {
            eprintln!("terrarium: --config is valid with --profile when starting a resumed turn");
            return 2;
        }
        if let Some(name) = profile.clone() {
            let cfg = match config::load(config_path.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("terrarium: {e}");
                    return 2;
                }
            };
            let selected = match cfg.resolve(&name) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("terrarium: {e}");
                    return 2;
                }
            };
            if let Err(e) = start_turn(
                &mut journal,
                &text,
                &selected,
                &inv.state,
                max_steps,
                timeout,
            ) {
                eprintln!("terrarium: {e}");
                return 1;
            }
        } else if let Some(previous) = previous {
            if let Err(e) = copy_turn(&mut journal, &text, &previous, &inv.state) {
                eprintln!("terrarium: {e}");
                return 1;
            }
        } else {
            let cfg = match config::load(config_path.as_deref()) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("terrarium: {e}");
                    return 2;
                }
            };
            let selected = match cfg.resolve(&cfg.default_profile) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("terrarium: {e}");
                    return 2;
                }
            };
            if let Err(e) = start_turn(
                &mut journal,
                &text,
                &selected,
                &inv.state,
                max_steps,
                timeout,
            ) {
                eprintln!("terrarium: {e}");
                return 1;
            }
        }
        if !is_resume {
            eprintln!("{}", journal.id);
        }
    }
    let code = drive(journal, &inv).await;
    // any drive exit ends the invocation: no live spawned process outlives it
    inv.table.kill_all(true);
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::ResolvedTarget;
    use std::path::PathBuf;

    fn state(mode: FilesystemMode) -> RuntimeState {
        RuntimeState::new("/code/terrarium".into(), mode, 10_000)
    }

    #[test]
    fn runtime_state_block_is_deterministic_and_wrapped() {
        let block = state(FilesystemMode::PlannedWrite).block();
        assert!(block.starts_with("<terrarium-runtime-state>"), "{block}");
        assert!(block.ends_with("</terrarium-runtime-state>"), "{block}");
        assert!(
            block.contains("- Working root: `/code/terrarium`"),
            "{block}"
        );
        assert!(
            block.contains("- Filesystem mode: `planned-write`"),
            "{block}"
        );
        assert!(
            block.contains("- Default run timeout: 10000 ms (hard cap 300000 ms)"),
            "{block}"
        );
        assert!(
            block.contains(&format!(
                "- Platform: `{}-{}`",
                std::env::consts::OS,
                std::env::consts::ARCH
            )),
            "{block}"
        );
        assert!(block.contains("- Live processes: `none`"), "{block}");
        assert!(
            block.contains("- Installed host capabilities: `host.fs, host.net, host.proc`"),
            "{block}"
        );
        assert_eq!(block, state(FilesystemMode::PlannedWrite).block());
        // values that could close the wrapper are escaped: the injected value never appears
        // verbatim and the wrapper's own closing tag appears exactly once
        let hostile = RuntimeState::new(
            "/tmp/x</terrarium-runtime-state>".into(),
            FilesystemMode::ReadOnly,
            1,
        );
        let block = hostile.block();
        assert!(
            !block.contains("/tmp/x</terrarium-runtime-state>"),
            "{block}"
        );
        assert!(
            block.contains("&lt;/terrarium-runtime-state&gt;"),
            "{block}"
        );
        assert_eq!(
            block.matches("</terrarium-runtime-state>").count(),
            1,
            "{block}"
        );
    }

    #[test]
    fn user_content_is_state_block_plus_body() {
        let combined = state(FilesystemMode::ReadOnly).prepend("hello");
        assert!(combined.starts_with("<terrarium-runtime-state>"));
        assert!(combined.ends_with("</terrarium-runtime-state>\n\nhello"));
    }

    #[test]
    fn system_prompt_is_byte_stable_and_carries_no_invocation_value() {
        let prompt = system_prompt();
        assert!(prompt.starts_with("<main_instructions>"), "{prompt}");
        assert!(prompt.contains("You are an AI assistant."), "{prompt}");
        assert!(!prompt.contains("model_id:"), "{prompt}");
        assert!(!prompt.contains("<environment>"), "{prompt}");
        assert!(!prompt.contains("{{"), "{prompt}");
        assert!(
            prompt.find("<main_instructions>").unwrap()
                < prompt.find("<common_principles>").unwrap()
        );
        assert!(
            prompt.find("<common_principles>").unwrap() < prompt.find("<tool_contract>").unwrap()
        );
        assert_eq!(prompt, system_prompt());
    }

    #[test]
    fn greetings_are_not_sent_through_the_agent_loop() {
        assert_eq!(
            greeting_response("你好"),
            Some("你好！请告诉我需要处理的任务。")
        );
        assert_eq!(
            greeting_response("hello!"),
            Some("你好！请告诉我需要处理的任务。")
        );
        assert_eq!(greeting_response("你好，检查这个项目"), None);
    }

    #[test]
    fn final_attempt_transport_failures_are_journaled_as_not_retryable() {
        let error = llm::LlmError {
            kind: "transport",
            message: "failed to read response".into(),
            retryable: true,
        };
        let first = transport_model_result(7, 1, &error);
        assert_eq!(first["error"]["retryable"], serde_json::json!(true));
        let second = transport_model_result(8, 2, &error);
        assert_eq!(second["error"]["retryable"], serde_json::json!(false));
        assert_eq!(second["requestSeq"], serde_json::json!(8));
    }

    #[test]
    fn only_closed_fences_are_programs() {
        let Extracted::Run { program, .. } = extract("notes\n```run\nreturn 42\n```") else {
            panic!("expected run");
        };
        assert_eq!(program.code, "return 42\n");
        assert!(matches!(extract("plain answer"), Extracted::NoRun));
        assert!(matches!(extract("```run\nreturn 42"), Extracted::Truncated));
        assert!(matches!(
            extract("```access\n{\"writes\":[],\"reason\":\"\"}\n```"),
            Extracted::NoRun
        ));
        assert!(matches!(
            extract("```run\nreturn 1\n```\n```run\nreturn 2\n```"),
            Extracted::Multiple
        ));
        assert!(matches!(
            extract("```access\n{}\n```\n```access\n{}\n```\n```run\nreturn 1\n```"),
            Extracted::Multiple
        ));
    }

    #[test]
    fn access_block_rules_match_the_contract() {
        let root = std::env::temp_dir();
        let file = root.join("terrarium-access-test-file.txt");
        std::fs::write(&file, "x").unwrap();
        let file_display = file.to_string_lossy().replace('\\', "/");
        let full = extract(&format!(
            "```access\n{{\"writes\":[\"{}\"],\"reason\":\"update the file\"}}\n```\n```run\nreturn 1\n```",
            file_display
        ));
        let Extracted::Run { access, .. } = full else {
            panic!("expected run");
        };
        let access = access.expect("access block");
        assert_eq!(access.writes, vec![file_display.clone()]);
        assert_eq!(access.reason, "update the file");
        let resolved = resolve_access_request(
            &access,
            FilesystemMode::PlannedWrite,
            &root.canonicalize().unwrap(),
        )
        .unwrap();
        assert_eq!(resolved.targets.len(), 1);
        assert_eq!(resolved.targets[0].display, file_display);
        assert!(!resolved.targets[0].parents_missing);

        // absent access block = empty request
        let missing = extract("```run\nreturn 1\n```");
        let Extracted::Run { access, .. } = missing else {
            panic!("expected run")
        };
        assert!(access.is_none());

        // invalid JSON / shapes are protocol errors, not empty requests
        assert!(matches!(
            extract("```access\nnot json\n```\n```run\nreturn 1\n```"),
            Extracted::InvalidAccess(_)
        ));
        assert!(matches!(
            extract("```access\n{\"writes\":[]}\n```\n```run\nreturn 1\n```"),
            Extracted::InvalidAccess(_)
        ));
        assert!(matches!(
            extract("```access\n{\"writes\":[],\"reason\":\"\",\"extra\":1}\n```\n```run\nreturn 1\n```"),
            Extracted::InvalidAccess(_)
        ));
        // access after run is ambiguous
        assert!(matches!(
            extract("```run\nreturn 1\n```\n```access\n{\"writes\":[],\"reason\":\"\"}\n```"),
            Extracted::AccessAfterRun
        ));
    }

    #[test]
    fn access_request_bounds_and_path_rules_are_enforced() {
        let root = std::env::temp_dir().join("terrarium-access-rules");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let working_root = root.canonicalize().unwrap();
        let existing = root.join("existing.txt");
        std::fs::write(&existing, "x").unwrap();
        let existing_display = existing.to_string_lossy().replace('\\', "/");

        let resolve = |writes: &[String], reason: &str, mode| {
            resolve_access_request(
                &AccessBlock {
                    writes: writes.to_vec(),
                    commands: Vec::new(),
                    reason: reason.into(),
                },
                mode,
                &working_root,
            )
        };
        // planned-write requires a reason for non-empty requests
        assert!(resolve(
            std::slice::from_ref(&existing_display),
            "",
            FilesystemMode::PlannedWrite
        )
        .is_err());
        assert!(resolve(
            std::slice::from_ref(&existing_display),
            "r",
            FilesystemMode::PlannedWrite
        )
        .is_ok());
        // other modes do not
        assert!(resolve(
            std::slice::from_ref(&existing_display),
            "",
            FilesystemMode::FullAccess
        )
        .is_ok());
        assert!(resolve(
            std::slice::from_ref(&existing_display),
            "",
            FilesystemMode::ReadOnly
        )
        .is_ok());

        let dir = root.to_string_lossy().replace('\\', "/");
        // directories, globs, relative paths, dot segments, duplicates
        assert!(
            resolve(std::slice::from_ref(&dir), "r", FilesystemMode::FullAccess)
                .unwrap_err()
                .contains("directory")
        );
        assert!(resolve(&[format!("{dir}/*.txt")], "r", FilesystemMode::FullAccess).is_err());
        assert!(resolve(
            &["relative.txt".to_string()],
            "r",
            FilesystemMode::FullAccess
        )
        .is_err());
        assert!(resolve(
            &[format!("{dir}/../x.txt")],
            "r",
            FilesystemMode::FullAccess
        )
        .is_err());
        assert!(resolve(
            &[existing_display.clone(), existing_display.clone()],
            "r",
            FilesystemMode::FullAccess
        )
        .unwrap_err()
        .contains("duplicate"));
        // new files resolve and are marked as parent-creating
        let resolved = resolve(
            &[format!("{dir}/new/deep/file.txt")],
            "r",
            FilesystemMode::FullAccess,
        )
        .unwrap();
        assert!(resolved.targets[0].parents_missing);
        // existing symlink targets are rejected for writes
        #[cfg(unix)]
        {
            let link = root.join("link.txt");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&existing, &link).unwrap();
            let error = resolve(
                &[link.to_string_lossy().into_owned()],
                "r",
                FilesystemMode::FullAccess,
            )
            .unwrap_err();
            assert!(error.contains("symbolic"), "{error}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn access_request_size_bounds_hold() {
        let too_many: Vec<String> = (0..33)
            .map(|index| format!("/tmp/file-{index}.txt"))
            .collect();
        assert!(parse_access_block(
            &serde_json::json!({"writes": too_many, "reason": "r"}).to_string()
        )
        .unwrap_err()
        .contains("32"));
        let long_reason = "r".repeat(201);
        assert!(parse_access_block(
            &serde_json::json!({"writes": [], "reason": long_reason}).to_string()
        )
        .unwrap_err()
        .contains("200"));
        let oversized = serde_json::json!({"writes": [format!("/tmp/{}.txt", "x".repeat(9000))], "reason": "r"}).to_string();
        assert!(parse_access_block(&oversized).unwrap_err().contains("8192"));
    }

    #[test]
    fn command_records_parse_resolve_and_deduplicate() {
        let root = std::env::temp_dir().join("terrarium-auth-commands");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let working_root = root.canonicalize().unwrap();
        let subdir = root.join("sub");
        std::fs::create_dir(&subdir).unwrap();
        let sh = if cfg!(windows) {
            crate::proc::resolve_executable("C:/Windows/System32/cmd.exe").unwrap()
        } else {
            crate::proc::resolve_executable("sh").unwrap()
        };
        let exe_name = if cfg!(windows) {
            "C:/Windows/System32/cmd.exe"
        } else {
            "sh"
        };
        let body = serde_json::json!({
            "writes": [],
            "commands": [
                {"exe": exe_name, "argv": ["/c", "echo hi"],
                 "cwd": subdir.to_string_lossy().replace('\\', "/")},
                {"exe": exe_name, "argv": ["/c", "echo hi"]},
                {"exe": exe_name, "argv": ["/c", "echo hi"],
                 "cwd": root.to_string_lossy().replace('\\', "/")},
            ],
            "reason": "run the suite",
        })
        .to_string();
        let block = parse_access_block(&body).unwrap();
        assert_eq!(block.commands.len(), 3);
        let resolved =
            resolve_access_request(&block, FilesystemMode::PlannedWrite, &working_root).unwrap();
        // the explicit working-root cwd resolves to the same identity as the cwd-less
        // default, so records two and three deduplicate; the subdir record stays distinct
        assert_eq!(resolved.commands.len(), 2);
        assert_eq!(resolved.commands[0].record.exe, sh);
        assert_eq!(
            resolved.commands[0].record.cwd,
            subdir.canonicalize().unwrap()
        );
        assert_eq!(resolved.commands[1].record.cwd, working_root);
        assert!(
            resolved.commands[0].display.contains("/c"),
            "{}",
            resolved.commands[0].display
        );
        assert!(
            resolved.commands[0].display.contains("(in "),
            "{}",
            resolved.commands[0].display
        );
        // planned-write requires a reason when only commands are requested
        let reasonless = serde_json::json!({
            "writes": [],
            "commands": [{"exe": "sh", "argv": ["-c"]}],
            "reason": "",
        })
        .to_string();
        let block = parse_access_block(&reasonless).unwrap();
        assert!(
            resolve_access_request(&block, FilesystemMode::PlannedWrite, &working_root)
                .unwrap_err()
                .contains("reason")
        );
        // bad shapes are protocol errors
        for bad in [
            serde_json::json!({"writes": [], "commands": [{"exe": "sh"}], "reason": "r"})
                .to_string(),
            serde_json::json!({"writes": [], "commands": [{"argv": []}], "reason": "r"})
                .to_string(),
            serde_json::json!({"writes": [], "commands": ["sh"], "reason": "r"}).to_string(),
            serde_json::json!({"writes": [], "commands": [], "reason": "r", "extra": 1})
                .to_string(),
        ] {
            assert!(parse_access_block(&bad).is_err(), "{bad}");
        }
        // unresolvable executables fail at resolution with a teachable error
        let missing = serde_json::json!({
            "writes": [],
            "commands": [{"exe": "definitely-not-a-real-tool-xyz", "argv": []}],
            "reason": "r",
        })
        .to_string();
        let block = parse_access_block(&missing).unwrap();
        assert!(
            resolve_access_request(&block, FilesystemMode::PlannedWrite, &working_root)
                .unwrap_err()
                .contains("definitely-not-a-real-tool-xyz")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    struct FixedAuthorizer(Decision);

    impl Authorizer for FixedAuthorizer {
        fn decide(&self, _request: &ResolvedAccessRequest) -> Decision {
            self.0
        }
    }

    fn launch(mode: FilesystemMode) -> LaunchOptions {
        LaunchOptions {
            mode,
            scopes: Vec::new(),
            execs: Vec::new(),
            offline: false,
        }
    }

    fn invocation_with(mode: FilesystemMode, decision: Decision) -> Invocation<'static> {
        let authorizer: &'static FixedAuthorizer = Box::leak(Box::new(FixedAuthorizer(decision)));
        invocation(
            launch(mode),
            "/tmp/terrarium-auth-inv".into(),
            PathBuf::from("/tmp/terrarium-auth-inv"),
            "terrarium-auth-inv",
            10_000,
            authorizer,
        )
    }

    fn invocation_at(
        mode: FilesystemMode,
        decision: Decision,
        root: &std::path::Path,
        scopes: Vec<WriteScope>,
    ) -> Invocation<'static> {
        let authorizer: &'static FixedAuthorizer = Box::leak(Box::new(FixedAuthorizer(decision)));
        invocation(
            LaunchOptions {
                scopes,
                ..launch(mode)
            },
            root.to_string_lossy().into_owned(),
            root.canonicalize().unwrap(),
            "terrarium-auth-inv",
            10_000,
            authorizer,
        )
    }

    fn invocation_exec_grant(
        mode: FilesystemMode,
        decision: Decision,
        root: &std::path::Path,
        grants: Vec<PathBuf>,
    ) -> Invocation<'static> {
        let authorizer: &'static FixedAuthorizer = Box::leak(Box::new(FixedAuthorizer(decision)));
        invocation(
            LaunchOptions {
                execs: grants,
                ..launch(mode)
            },
            root.to_string_lossy().into_owned(),
            root.canonicalize().unwrap(),
            "terrarium-auth-inv",
            10_000,
            authorizer,
        )
    }

    fn run_action(writes: &[String], reason: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "run",
            "source": "return 1",
            "timeoutMs": 100,
            "access": {"writes": writes, "reason": reason}
        })
    }

    fn command_action(commands: serde_json::Value, reason: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "run",
            "source": "return 1",
            "timeoutMs": 100,
            "access": {"writes": [], "commands": commands, "reason": reason}
        })
    }

    #[test]
    fn planned_write_lifecycle_subtracts_prompts_and_freezes() {
        let root = std::env::temp_dir().join("terrarium-auth-lifecycle");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let inside = root.join("inside.txt");
        std::fs::write(&inside, "x").unwrap();
        // a sibling of the scoped root: outside every operator scope, but a clean absolute
        // path (lexical `..` escapes are rejected at validation and would test the wrong rule)
        let outside = std::env::temp_dir().join("terrarium-auth-outside.txt");
        std::fs::write(&outside, "x").unwrap();
        let root_display = root.to_string_lossy().replace('\\', "/");
        let scope = WriteScope::from_operator_spec(&root_display).unwrap();
        let inside_display = inside.to_string_lossy().replace('\\', "/");
        let outside_display = outside.to_string_lossy().replace('\\', "/");

        // covered: operator scope absorbs the request, no prompt, no approved exacts
        let inv = invocation_at(
            FilesystemMode::PlannedWrite,
            Decision::Deny,
            &root,
            vec![scope.clone()],
        );
        let covered = authorize_run_access(
            &inv,
            &run_action(std::slice::from_ref(&inside_display), "edit"),
            1,
            1,
        );
        assert_eq!(covered.event["decision"], "covered");
        let authority = covered.authority.expect("covered runs");
        assert!(authority
            .authorize_write("display", &inside.canonicalize().unwrap())
            .is_ok());

        // remainder prompts: denial ends the run with a structured observation
        let denied = authorize_run_access(
            &inv,
            &run_action(std::slice::from_ref(&outside_display), "edit outside"),
            1,
            2,
        );
        assert!(denied.authority.is_none());
        assert_eq!(denied.event["decision"], "deny");
        let observation = denied.event["observation"].as_str().unwrap();
        assert!(
            observation.contains("authorization_denied"),
            "{observation}"
        );
        assert!(
            observation.contains("denied the requested write and command set"),
            "{observation}"
        );

        // approval freezes operator scopes plus the approved exact path
        let allowing_inv = invocation_at(
            FilesystemMode::PlannedWrite,
            Decision::Allow,
            &root,
            vec![scope.clone()],
        );
        let allowed = authorize_run_access(
            &allowing_inv,
            &run_action(
                &[inside_display.clone(), outside_display.clone()],
                "edit both",
            ),
            1,
            3,
        );
        assert_eq!(allowed.event["decision"], "allow");
        let authority = allowed.authority.expect("approved run");
        assert!(authority
            .authorize_write("display", &outside.canonicalize().unwrap())
            .is_ok());
        assert!(authority
            .authorize_write("display", &inside.canonicalize().unwrap())
            .is_ok());
        // a path outside every frozen scope stays denied even after an approval
        let bystander = std::env::temp_dir().join("terrarium-auth-bystander.txt");
        std::fs::write(&bystander, "x").unwrap();
        let bystander_identity = bystander.canonicalize().unwrap();
        let _ = std::fs::remove_file(&bystander);
        assert!(authority
            .authorize_write("display", &bystander_identity)
            .is_err());

        // unavailable: no interactive authorizer in this invocation
        let unavailable_inv = invocation_at(
            FilesystemMode::PlannedWrite,
            Decision::Unavailable,
            &root,
            vec![scope],
        );
        let unavailable = authorize_run_access(
            &unavailable_inv,
            &run_action(std::slice::from_ref(&outside_display), "edit"),
            1,
            4,
        );
        assert!(unavailable.authority.is_none());
        assert_eq!(unavailable.event["decision"], "unavailable");
        assert!(unavailable.event["observation"]
            .as_str()
            .unwrap()
            .contains("no interactive authorizer"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn command_lifecycle_grants_subtract_and_freeze() {
        let root = std::env::temp_dir().join("terrarium-auth-cmd");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let working_root = root.canonicalize().unwrap();
        let exe_name = if cfg!(windows) {
            "C:/Windows/System32/cmd.exe"
        } else {
            "sh"
        };
        let resolved_exe = crate::proc::resolve_executable(exe_name).unwrap();
        let action = command_action(
            serde_json::json!([{ "exe": exe_name, "argv": ["/c", "echo hi"] }]),
            "run the suite",
        );

        // planned-write prompts for the undeclared remainder and freezes the approved record
        let inv = invocation_at(
            FilesystemMode::PlannedWrite,
            Decision::Allow,
            &root,
            Vec::new(),
        );
        let approved = authorize_run_access(&inv, &action, 1, 1);
        assert_eq!(approved.event["decision"], "allow");
        assert!(approved.event["commands"].is_array());
        assert!(approved.event["commands"][0]
            .as_str()
            .unwrap()
            .contains("echo hi"));
        let proc = approved.proc_authority.expect("frozen process authority");
        assert!(proc
            .authorize(
                exe_name,
                &["/c".into(), "echo hi".into()],
                None,
                &working_root
            )
            .is_ok());
        // a different argv or cwd is not the approved command
        assert!(proc
            .authorize(
                exe_name,
                &["/c".into(), "echo bye".into()],
                None,
                &working_root
            )
            .is_err());

        // an operator --allow-exec grant absorbs the request without a prompt
        let granted = invocation_exec_grant(
            FilesystemMode::PlannedWrite,
            Decision::Deny,
            &root,
            vec![resolved_exe.clone()],
        );
        let covered = authorize_run_access(&granted, &action, 1, 2);
        assert_eq!(covered.event["decision"], "covered");
        let proc = covered.proc_authority.expect("grants freeze as records");
        assert!(proc
            .authorize(
                exe_name,
                &["/c".into(), "anything".into()],
                None,
                &working_root
            )
            .is_ok());

        // denial ends the run with corrective feedback
        let denied_inv = invocation_at(
            FilesystemMode::PlannedWrite,
            Decision::Deny,
            &root,
            Vec::new(),
        );
        let denied = authorize_run_access(&denied_inv, &action, 1, 3);
        assert!(denied.authority.is_none() && denied.proc_authority.is_none());
        assert_eq!(denied.event["decision"], "deny");
        assert!(denied.event["observation"]
            .as_str()
            .unwrap()
            .contains("command set"));

        // read-only denies commands as write-class effects
        let read_only = invocation_at(FilesystemMode::ReadOnly, Decision::Allow, &root, Vec::new());
        let denied = authorize_run_access(&read_only, &action, 1, 4);
        assert!(denied.authority.is_none());
        assert!(denied.event["observation"]
            .as_str()
            .unwrap()
            .contains("process launch"));

        // full-access journals the declaration and freezes unrestricted commands
        let full = invocation_at(
            FilesystemMode::FullAccess,
            Decision::Deny,
            &root,
            Vec::new(),
        );
        let declared = authorize_run_access(&full, &action, 1, 5);
        assert_eq!(declared.event["decision"], "declared");
        assert_eq!(
            declared.proc_authority.expect("unrestricted"),
            ProcAuthority::Unrestricted
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn mode_specific_declarations_are_journaled_not_enforced() {
        let root = std::env::temp_dir().join("terrarium-auth-modes");
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let display = file.to_string_lossy().replace('\\', "/");
        let action = run_action(&[display], "r");

        let full = invocation_with(FilesystemMode::FullAccess, Decision::Deny);
        let declared = authorize_run_access(&full, &action, 1, 1);
        assert_eq!(declared.event["decision"], "declared");
        assert!(declared.event.get("observation").is_none());
        assert_eq!(
            declared.authority.expect("full access runs"),
            RunFilesystemAuthority::FullAccess
        );

        let read_only = invocation_with(FilesystemMode::ReadOnly, Decision::Allow);
        let denied = authorize_run_access(&read_only, &action, 1, 2);
        assert!(denied.authority.is_none());
        assert_eq!(denied.event["decision"], "deny");
        assert!(denied.event["observation"]
            .as_str()
            .unwrap()
            .contains("read-only"));

        // empty request never journals an event and runs under the base authority
        let empty = run_action(&[], "");
        let quiet = authorize_run_access(&read_only, &empty, 1, 3);
        assert!(quiet.event.is_null());
        assert_eq!(quiet.authority.unwrap(), RunFilesystemAuthority::ReadOnly);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dispositions_are_strict_and_protocol_errors_stay_model_bound() {
        assert_eq!(
            parse_disposition(Some(serde_json::json!({
                "to": "model",
                "facts": {"count": 2}
            })))
            .unwrap(),
            serde_json::json!({"to":"model","facts":{"count":2}})
        );
        assert_eq!(
            parse_disposition(Some(serde_json::json!({
                "to": "user",
                "message": "done"
            })))
            .unwrap(),
            serde_json::json!({"to":"user","message":"done"})
        );
        for value in [
            serde_json::json!("done"),
            serde_json::json!({"to": "model"}),
            serde_json::json!({"to": "model","facts":"large text"}),
            serde_json::json!({"to": "user","message":"done","facts":{}}),
            serde_json::json!({"to": "agent","facts":{}}),
        ] {
            assert!(parse_disposition(Some(value)).is_err());
        }
        // a run with no returned value gets the teachable variant, not a bare schema complaint
        let missing = parse_disposition(None).unwrap_err();
        assert!(missing.contains("top-level return"), "{missing}");
        assert!(missing.contains("async IIFE"), "{missing}");

        let observation = observation_for_extract(3, 4, &Extracted::NoRun).unwrap();
        let value: serde_json::Value = serde_json::from_str(&observation).unwrap();
        assert_eq!(value["to"], "model");
        assert_eq!(value["turn"], 3);
        assert_eq!(value["step"], 4);
        assert_eq!(value["error"]["kind"], "protocol");
    }

    #[test]
    fn interrupted_model_requests_retry_only_the_first_attempt() {
        for (attempt, retryable) in [(1, true), (2, false)] {
            let request = Event {
                kind: "model/request".into(),
                seq: 7,
                ts: None,
                data: serde_json::json!({"step": 3, "attempt": attempt}),
            };
            let result = interrupted_model_result(&request);
            assert_eq!(result["requestSeq"], 7);
            assert_eq!(result["ok"], false);
            assert_eq!(result["error"]["kind"], "interrupted");
            assert_eq!(result["error"]["retryable"], retryable);
        }
    }

    #[test]
    fn model_facts_allow_realistic_code_summaries_but_remain_bounded() {
        let within_limit = "x".repeat(15 * 1024);
        assert!(parse_disposition(Some(serde_json::json!({
            "to": "model",
            "facts": {"text": within_limit}
        })))
        .is_ok());

        let oversized = "x".repeat(16 * 1024 + 1);
        let error = parse_disposition(Some(serde_json::json!({
            "to": "model",
            "facts": {"text": oversized}
        })))
        .unwrap_err();
        assert!(error.contains("16384"), "{error}");
    }

    #[test]
    fn oversized_model_facts_are_rejected_before_model_projection() {
        let oversized = "x".repeat(16 * 1024 + 1);
        let error = parse_disposition(Some(serde_json::json!({
            "to": "model",
            "facts": {"text": oversized}
        })))
        .unwrap_err();
        assert!(error.contains("facts"), "{error}");
        assert!(error.contains("limit"), "{error}");
    }

    #[test]
    fn model_observation_contains_only_bounded_disposition_facts() {
        let outcome = Outcome {
            ok: true,
            value: None,
            error: None,
            termination: crate::Termination::Returned,
            stdout: String::new(),
            timed_out: false,
            elapsed_ms: 1,
            writes: Vec::new(),
            writes_truncated: false,
            receipts: Vec::new(),
        };
        let observation = model_observation_with_writes(
            2,
            5,
            &serde_json::json!({"to":"model","facts":{"path":"/work/project/report.txt"}}),
            &outcome,
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&observation).unwrap(),
            serde_json::json!({
                "turn": 2,
                "step": 5,
                "to": "model",
                "facts": {"path": "/work/project/report.txt"}
            })
        );
    }

    #[test]
    fn resolved_target_shape_is_display_identity_and_missing_parents() {
        let target = ResolvedTarget {
            display: "/tmp/terrarium-target.txt".into(),
            identity: PathBuf::from("/tmp/terrarium-target.txt"),
            parents_missing: true,
        };
        let request = ResolvedAccessRequest {
            targets: vec![target],
            commands: Vec::new(),
            reason: "why".into(),
        };
        assert_eq!(request.displays(), vec!["/tmp/terrarium-target.txt"]);
        assert_eq!(request.reason, "why");
    }

    #[test]
    fn resolve_invocation_rejects_flag_combinations() {
        assert!(resolve_invocation(true, true, &[], &[], false).is_err());
        assert!(resolve_invocation(true, false, &["/tmp".into()], &[], false).is_err());
        assert!(resolve_invocation(false, true, &["/tmp".into()], &[], false).is_err());
        assert!(resolve_invocation(false, false, &[], &[], false).is_ok());
        assert!(resolve_invocation(false, false, &[], &[], true).is_ok());
        assert!(resolve_invocation(
            false,
            false,
            &["/definitely/not/a/real/path".into()],
            &[],
            false
        )
        .is_err());
        // --allow-exec is likewise a planned-write-only grant
        assert!(resolve_invocation(true, false, &[], &["sh".into()], false).is_err());
        assert!(resolve_invocation(false, true, &[], &["sh".into()], false).is_err());
        assert!(resolve_invocation(false, false, &[], &["sh".into()], false).is_ok());
        assert!(resolve_invocation(
            false,
            false,
            &[],
            &["definitely-not-a-real-tool-xyz".into()],
            false
        )
        .is_err());
    }

    #[test]
    fn journaled_decisions_rebuild_authority_or_block_on_resume() {
        let root = std::env::temp_dir().join("terrarium-auth-resume");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        let display = file.to_string_lossy().replace('\\', "/");
        // the invocation's operator scopes cover a different tree, so an "allow" rebuild
        // must add the approved exact path itself
        let inv = invocation_with(FilesystemMode::PlannedWrite, Decision::Allow);
        let action = run_action(std::slice::from_ref(&display), "edit");

        // "declared" is journaled only under full-access: it rebuilds nothing in a
        // planned-write invocation — the mode gate blocks the scoped-to-full escalation in
        // both directions
        assert_eq!(
            authority_from_journaled_decision(&inv, &action, "declared"),
            None
        );
        assert!(matches!(
            authority_from_journaled_decision(&inv, &action, "covered"),
            Some((RunFilesystemAuthority::Scoped(_), ProcAuthority::Allowed(_)))
        ));
        let (allowed, procs) = authority_from_journaled_decision(&inv, &action, "allow")
            .expect("allow rebuilds the frozen authority");
        assert!(allowed
            .authorize_write("display", &file.canonicalize().unwrap())
            .is_ok());
        assert_eq!(procs, ProcAuthority::Allowed(Default::default()));
        // a mode change on resume never rebuilds authority from the journal: read-only
        // executes no pending run regardless of the recorded decision, and full-access does
        // not inherit a scoped planned-write decision
        let read_only = invocation_with(FilesystemMode::ReadOnly, Decision::Deny);
        let full_access = invocation_with(FilesystemMode::FullAccess, Decision::Deny);
        for other in [&read_only, &full_access] {
            assert!(authority_from_journaled_decision(other, &action, "allow").is_none());
            assert!(authority_from_journaled_decision(other, &action, "covered").is_none());
        }
        assert!(authority_from_journaled_decision(&read_only, &action, "declared").is_none());
        assert_eq!(
            authority_from_journaled_decision(&full_access, &action, "declared"),
            Some((
                RunFilesystemAuthority::FullAccess,
                ProcAuthority::Unrestricted
            ))
        );
        // blocking decisions never rebuild
        for decision in ["deny", "cancel", "unavailable", "invalid"] {
            assert!(authority_from_journaled_decision(&inv, &action, decision).is_none());
        }
        // a target that no longer resolves (it became a directory) rebuilds nothing —
        // the resume path falls through to a fresh decision instead
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(authority_from_journaled_decision(&inv, &action, "allow").is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn journaling_more_receipts_than_the_cap_leaves_a_visible_marker() {
        // a journal is opened: serialize against other journal-holding tests
        let _journal_guard = crate::session::tests::STATE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let state =
            std::env::temp_dir().join(format!("terrarium-agent-receipts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let root = state.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("XDG_STATE_HOME", &state);
        let mut journal = crate::session::Journal::create(&root).unwrap();
        journal
            .append(
                "turn/start",
                crate::session::turn_data(
                    "test",
                    "system",
                    &crate::llm::test_profile(
                        "openai-chat-completions",
                        "https://example.test",
                        "test-model",
                    ),
                    2,
                    100,
                    300,
                ),
            )
            .unwrap();
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":1}))
            .unwrap();
        journal
            .append(
                "model/result",
                serde_json::json!({"requestSeq":2,"ok":true,"content":"```run\nreturn 1\n```","action":{"kind":"run","source":"return 1\n","timeoutMs":1,"access":{"writes":[],"reason":""}}}),
            )
            .unwrap();
        journal
            .append("run/start", serde_json::json!({"modelResultSeq":3}))
            .unwrap();
        let receipts: Vec<serde_json::Value> = (0..RUN_RECEIPT_CAP + 5)
            .map(|index| {
                serde_json::json!({
                    "method": "GET",
                    "url": format!("https://example.test/{index}"),
                    "status": 200,
                    "bytes": 1,
                })
            })
            .collect();
        journal_receipts(&mut journal, Some(4), &receipts).unwrap();
        let journaled = journal
            .events
            .iter()
            .filter(|event| event.kind == "net/request")
            .count();
        assert_eq!(journaled, RUN_RECEIPT_CAP);
        // the audit trail stays honest: what was dropped is counted, not hidden
        let marker = journal
            .events
            .iter()
            .rev()
            .find(|event| event.kind == "receipts/truncated")
            .expect("truncation marker");
        assert_eq!(marker.data["dropped"], serde_json::json!(5));
        assert_eq!(marker.data["runSeq"], serde_json::json!(4));
        drop(journal);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn the_cap_never_drops_a_spawn_anchor_receipt() {
        // a journal is opened: serialize against other journal-holding tests
        let _journal_guard = crate::session::tests::STATE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let state =
            std::env::temp_dir().join(format!("terrarium-agent-anchors-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&state);
        let root = state.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("XDG_STATE_HOME", &state);
        let mut journal = crate::session::Journal::create(&root).unwrap();
        journal
            .append(
                "turn/start",
                crate::session::turn_data(
                    "test",
                    "system",
                    &crate::llm::test_profile(
                        "openai-chat-completions",
                        "https://example.test",
                        "test-model",
                    ),
                    2,
                    100,
                    300,
                ),
            )
            .unwrap();
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":1}))
            .unwrap();
        journal
            .append(
                "model/result",
                serde_json::json!({"requestSeq":2,"ok":true,"content":"```run\nreturn 1\n```","action":{"kind":"run","source":"return 1\n","timeoutMs":1,"access":{"writes":[],"reason":""}}}),
            )
            .unwrap();
        journal
            .append("run/start", serde_json::json!({"modelResultSeq":3}))
            .unwrap();
        // a busy run exhausts the cap with exec receipts, then spawns one process: the
        // handle-bearing receipt must survive the cap, or the process's later exit
        // receipt would fail validation and abort the session
        let mut receipts: Vec<serde_json::Value> = Vec::new();
        for _ in 0..RUN_RECEIPT_CAP + 1 {
            receipts.push(serde_json::json!({"code": 0, "tail": "hi"}));
        }
        receipts.push(serde_json::json!({
            "exe": "/bin/sh", "argv": ["-c", "sleep 0.1"], "cwd": "/tmp", "pid": 4242,
            "handle": "p1", "log": "/state/terrarium/sessions/s/procs/p1.log",
        }));
        journal_receipts(&mut journal, Some(4), &receipts).unwrap();
        assert!(journal
            .events
            .iter()
            .any(|event| event.kind == "run/spawn" && event.data["handle"].as_str() == Some("p1")));
        // the marker still counts exactly the capped receipts — here just the one exec
        // exit beyond the cap, not the exempt spawn anchor
        let marker = journal
            .events
            .iter()
            .rev()
            .find(|event| event.kind == "receipts/truncated")
            .expect("truncation marker");
        assert_eq!(marker.data["dropped"], serde_json::json!(1));
        // the anchored exit receipt journals cleanly instead of failing validation
        journal_exit_receipts(
            &mut journal,
            &[serde_json::json!({"handle": "p1", "code": 0, "tail": ""})],
        )
        .unwrap();
        drop(journal);
        let _ = std::fs::remove_dir_all(&state);
    }
}
