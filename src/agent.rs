//! Durable model-driven agent loop.

use tokio::sync::watch;

use crate::{
    add_mount, config, eval_js,
    fs::Mount,
    llm,
    session::{project, turn_data, Event, Journal},
    ErrorKind, Outcome, MAX_TIMEOUT_MS,
};

const COMMON: &str = include_str!("prompts/common.md");
const ROLE_TEMPLATE: &str = include_str!("prompts/main.md");
const RUN_TIMEOUT_DEFAULT_MS: u64 = 10_000;
const DEFAULT_MAX_STEPS: u64 = 256;
const FEEDBACK_CAP: usize = 12 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    ReadOnly,
    Workspace,
    Full,
}

impl AccessMode {
    fn parse(read_only: bool, full: bool) -> Result<Self, String> {
        if read_only && full {
            return Err("--read-only and --full-access are mutually exclusive".into());
        }
        Ok(if read_only {
            Self::ReadOnly
        } else if full {
            Self::Full
        } else {
            Self::Workspace
        })
    }
}

pub struct RunProgram {
    pub code: String,
    pub timeout_ms: Option<u64>,
}

/// Only standalone run-fence lines participate in the protocol.
fn scan_run_fences(reply: &str) -> (Vec<String>, bool) {
    let mut blocks = Vec::new();
    let mut body: Option<String> = None;
    for line in reply.lines() {
        match (line.trim(), body.is_some()) {
            ("```", true) => {
                blocks.push(body.take().unwrap().trim_start_matches('\n').to_string());
            }
            ("```run", false) => body = Some(String::new()),
            _ => {
                if let Some(source) = body.as_mut() {
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
    Run(RunProgram),
    NoRun,
    Truncated,
    Multiple,
}

pub(crate) fn extract(reply: &str) -> Extracted {
    let (blocks, unclosed) = scan_run_fences(reply);
    match (blocks.as_slice(), unclosed) {
        ([], false) => Extracted::NoRun,
        ([], true) => Extracted::Truncated,
        ([code], false) => Extracted::Run(RunProgram {
            timeout_ms: parse_timeout_directive(code),
            code: code.clone(),
        }),
        _ => Extracted::Multiple,
    }
}

fn feedback(out: &Outcome) -> String {
    let payload = serde_json::json!({
        "ok": out.ok,
        "value": out.value,
        "stdout": out.stdout,
        "error": out.error,
        "termination": out.termination,
        "timedOut": out.timed_out,
        "elapsedMs": out.elapsed_ms,
    });
    if payload.to_string().len() <= FEEDBACK_CAP {
        return payload.to_string();
    }
    serde_json::json!({
        "ok": false,
        "value": null,
        "stdout": crate::truncate_utf8(&out.stdout, 4096),
        "error": {"kind": ErrorKind::Protocol, "message": "run result exceeded the feedback limit"},
        "termination": "failed",
        "timedOut": false,
        "elapsedMs": out.elapsed_ms,
        "truncated": true,
    })
    .to_string()
}

fn observation_for_extract(extracted: &Extracted) -> Option<String> {
    Some(match extracted {
        Extracted::Truncated => {
            "protocol error: no program was executed; close the single ```run block and send one complete program with no prose or other code block".into()
        }
        Extracted::Multiple => {
            "protocol error: no program was executed; the response contained multiple run blocks; combine the work into exactly one complete ```run program".into()
        }
        Extracted::NoRun => {
            "protocol error: no program was executed; send exactly one complete ```run program with no prose or other code block".into()
        }
        Extracted::Run(_) => return None,
    })
}

fn system_prompt(
    profile: &config::ResolvedProfile,
    root: &str,
    timeout: u64,
    mounts: &[Mount],
) -> String {
    let role = ROLE_TEMPLATE
        .replace("{{RUN_DEFAULT_MS}}", &timeout.to_string())
        .replace("{{RUN_CAP_MS}}", &MAX_TIMEOUT_MS.to_string())
        .replace("{{MODEL}}", &profile.model);
    format!(
        "{}\n\n<environment>\nWorking root: {}\n\n{}\n</environment>\n\n{}\n\n{}",
        role,
        root,
        access_guidance(mounts),
        COMMON,
        crate::contract_for(mounts, &profile.model),
    )
}

fn access_guidance(mounts: &[Mount]) -> String {
    if mounts.iter().any(|mount| mount.virtual_path() == "/") {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(|path| path.to_string_lossy().into_owned());
        let home_note = home
            .map(|path| format!(" The current user's home directory is `{path}`."))
            .unwrap_or_default();
        format!(
            "Filesystem root `/` is accessible. Use real absolute paths, including paths under the current user's home directory.{home_note} `~` is not expanded by JavaScript. If a path is denied, report the denial and do not retry the same path or invent another mount."
        )
    } else {
        let roots = mounts
            .iter()
            .map(|mount| mount.virtual_path().trim_end_matches('/'))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "Accessible virtual roots: {roots}. The session working root is `/workspace`; use these virtual paths exactly. `~` is not expanded by JavaScript. If a requested path is outside these roots, report that it is not authorized and do not retry alternate spellings or invent another mount. The operator must authorize another location with `--mount` or `--full-access`."
        )
    }
}

fn invocation_mounts(
    access: AccessMode,
    root: &str,
    explicit: Vec<Mount>,
) -> Result<Vec<Mount>, String> {
    let mut mounts = if access == AccessMode::Full {
        vec![Mount::new("/", "/", true)?]
    } else {
        vec![Mount::new(
            "/workspace",
            root,
            access == AccessMode::Workspace,
        )?]
    };
    for mount in explicit {
        if mounts.iter().any(|existing| existing.overlaps(&mount)) {
            return Err(format!(
                "overlapping mount virtual roots are not allowed: {}",
                mount.virtual_path().trim_end_matches('/')
            ));
        }
        mounts.push(mount);
    }
    mounts.sort_by_key(|mount| std::cmp::Reverse(mount.virtual_path().len()));
    Ok(mounts)
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
    root: &str,
    max_steps: u64,
    timeout: u64,
    mounts: &[Mount],
) -> Result<(), String> {
    let prompt = system_prompt(profile, root, timeout, mounts);
    journal.append(
        "turn/start",
        turn_data(
            message,
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
    root: &str,
    mounts: &[Mount],
) -> Result<(), String> {
    let mut data = previous.data.clone();
    let profile = profile_from_turn(previous);
    let (timeout, _) = turn_timeouts(previous);
    data["message"] = serde_json::Value::String(message.into());
    data["systemPrompt"] =
        serde_json::Value::String(system_prompt(&profile, root, timeout, mounts));
    journal.append("turn/start", data)?;
    Ok(())
}

async fn execute_run(
    journal: &mut Journal,
    run_seq: u64,
    action: &serde_json::Value,
    mounts: &[Mount],
    limits: (u64, u64),
) -> Result<Option<String>, String> {
    let source = action["source"]
        .as_str()
        .ok_or_else(|| "run action has no source".to_string())?;
    let timeout = action["timeoutMs"]
        .as_u64()
        .unwrap_or(limits.0)
        .min(limits.1)
        .min(MAX_TIMEOUT_MS);
    let (cancel_tx, _cancel_rx) = watch::channel(false);
    let outcome = eval_js(source, timeout, mounts, cancel_tx).await;
    let answer = outcome.answer.clone();
    let observation = if answer.is_none() {
        Some(feedback(&outcome))
    } else {
        None
    };
    let mut data = serde_json::json!({
        "runSeq": run_seq,
        "status": "completed",
        "outcome": {
            "ok": outcome.ok,
            "value": outcome.value,
            "answer": outcome.answer,
            "stdout": outcome.stdout,
            "error": outcome.error,
            "termination": outcome.termination,
            "timedOut": outcome.timed_out,
            "elapsedMs": outcome.elapsed_ms,
        },
    });
    if let Some(observation) = observation {
        data["observation"] = serde_json::Value::String(observation);
    }
    journal.append("run/result", data)?;
    Ok(answer)
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
    content: String,
    default_timeout: u64,
    max_timeout: u64,
) -> Result<(), String> {
    let extracted = extract(&content);
    let action = if let Some(message) = observation_for_extract(&extracted) {
        serde_json::json!({"kind":"observation","message":message})
    } else if let Extracted::Run(program) = extracted {
        serde_json::json!({
            "kind": "run",
            "source": program.code,
            "timeoutMs": program.timeout_ms.unwrap_or(default_timeout).min(max_timeout),
        })
    } else {
        unreachable!()
    };
    journal.append(
        "model/result",
        serde_json::json!({"requestSeq":request_seq,"ok":true,"content":content,"action":action}),
    )?;
    Ok(())
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
) -> Result<(), String> {
    let request_seq = journal.append(
        "model/request",
        serde_json::json!({"step":step,"attempt":attempt}),
    )?;
    let messages = project(&journal.events, request_seq);
    let profile = profile_from_turn(turn);
    match llm::complete(&profile, messages).await {
        Ok(content) => {
            let limits = turn_timeouts(turn);
            process_model_result(journal, request_seq, content, limits.0, limits.1)
        }
        Err(error) => {
            journal.append(
                "model/result",
                serde_json::json!({
                    "requestSeq": request_seq,
                    "ok": false,
                    "error": {"kind": error.kind, "message": error.message, "retryable": error.retryable}
                }),
            )?;
            Ok(())
        }
    }
}

async fn drive(mut journal: Journal, mounts: &[Mount]) -> i32 {
    loop {
        let Some(turn) = journal.open_turn().cloned() else {
            return 1;
        };
        let Some(last) = journal.events.last().cloned() else {
            return 1;
        };
        match last.kind.as_str() {
            "model/request" => {
                if journal.events.iter().any(|event| {
                    event.kind == "model/result"
                        && event.data["requestSeq"].as_u64() == Some(last.seq)
                }) {
                    continue;
                }
                if journal
                    .append(
                        "model/result",
                        serde_json::json!({
                            "requestSeq": last.seq,
                            "ok": false,
                            "error": {"kind":"interrupted", "message":"request interrupted before result was durable", "retryable":true}
                        }),
                    )
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
                    if model_attempt(&mut journal, &turn, step, 2).await.is_err() {
                        return 1;
                    }
                    continue;
                }
                let reason = if last.data["error"]["kind"] == "cancelled" {
                    "cancelled"
                } else {
                    "failed"
                };
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
                let existing_seq = journal.events.iter().find_map(|event| {
                    (event.kind == "run/start"
                        && event.data["modelResultSeq"].as_u64() == Some(last.seq))
                    .then_some(event.seq)
                });
                if let Some(run_start_seq) = existing_seq {
                    if !journal.events.iter().any(|event| {
                        event.kind == "run/result"
                            && event.data["runSeq"].as_u64() == Some(run_start_seq)
                    }) && recover_unknown_run(&mut journal, run_start_seq).is_err()
                    {
                        return 1;
                    }
                    continue;
                }
                let run_seq = match journal
                    .append("run/start", serde_json::json!({"modelResultSeq":last.seq}))
                {
                    Ok(seq) => seq,
                    Err(_) => return 1,
                };
                let limits = turn_timeouts(&turn);
                match execute_run(&mut journal, run_seq, &last.data["action"], mounts, limits).await
                {
                    Ok(Some(answer)) => {
                        if journal
                            .append(
                                "turn/end",
                                serde_json::json!({"reason":"answered","answerRunSeq":run_seq}),
                            )
                            .is_err()
                        {
                            return 1;
                        }
                        println!("{answer}");
                        return 0;
                    }
                    Ok(None) => {}
                    Err(_) => return 1,
                }
                continue;
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
                if last.data["status"] == "completed"
                    && last.data["outcome"]["answer"].as_str().is_some()
                {
                    let answer_run_seq = last.data["runSeq"].as_u64().unwrap_or(0);
                    let answer = last.data["outcome"]["answer"].as_str().unwrap_or_default();
                    if journal
                        .append(
                            "turn/end",
                            serde_json::json!({"reason":"answered","answerRunSeq":answer_run_seq}),
                        )
                        .is_err()
                    {
                        return 1;
                    }
                    println!("{answer}");
                    return 0;
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
        if model_attempt(&mut journal, &turn, step, 1).await.is_err() {
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

pub async fn run_cli(args: &[String]) -> i32 {
    let mut profile: Option<String> = None;
    let mut config_path: Option<std::path::PathBuf> = None;
    let mut resume: Option<String> = None;
    let mut read_only = false;
    let mut full = false;
    let mut max_steps = DEFAULT_MAX_STEPS;
    let mut timeout = RUN_TIMEOUT_DEFAULT_MS;
    let mut explicit_mounts = Vec::new();
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
            "--mount" if i + 1 < args.len() => {
                if let Err(error) = add_mount(&mut explicit_mounts, &args[i + 1]) {
                    eprintln!("terrarium: {error}");
                    return 2;
                }
                i += 2;
            }
            "--mount" => {
                eprintln!("terrarium: --mount expects /virtual=real[:rw]");
                return 2;
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
    let access = match AccessMode::parse(read_only, full) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("terrarium: {e}");
            return 2;
        }
    };
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
    let (mut journal, mounts) = if let Some(id) = &resume {
        let journal = match Journal::open(id) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("terrarium: {e}");
                return 2;
            }
        };
        let root = journal.header.working_root.display_path.clone();
        let mounts = match invocation_mounts(access, &root, explicit_mounts) {
            Ok(mounts) => mounts,
            Err(error) => {
                eprintln!("terrarium: {error}");
                return 2;
            }
        };
        (journal, mounts)
    } else {
        let root = match std::env::current_dir() {
            Ok(root) => root,
            Err(error) => {
                eprintln!("terrarium: cannot determine working root: {error}");
                return 2;
            }
        };
        let root_display = root.to_string_lossy().into_owned();
        let mounts = match invocation_mounts(access, &root_display, explicit_mounts) {
            Ok(mounts) => mounts,
            Err(error) => {
                eprintln!("terrarium: {error}");
                return 2;
            }
        };
        let journal = match Journal::create(&root) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("terrarium: {e}");
                return 2;
            }
        };
        (journal, mounts)
    };
    let root = journal.header.working_root.display_path.clone();
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
                &root,
                max_steps,
                timeout,
                &mounts,
            ) {
                eprintln!("terrarium: {e}");
                return 1;
            }
        } else if let Some(previous) = previous {
            if let Err(e) = copy_turn(&mut journal, &text, &previous, &root, &mounts) {
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
                &root,
                max_steps,
                timeout,
                &mounts,
            ) {
                eprintln!("terrarium: {e}");
                return 1;
            }
        }
        if !is_resume {
            eprintln!("{}", journal.id);
        }
    }
    drive(journal, &mounts).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_access_uses_filesystem_root_and_prompt_lists_denials() {
        let mounts = invocation_mounts(AccessMode::Full, "/tmp", Vec::new()).unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].virtual_path(), "/");
        let profile = config::ResolvedProfile {
            name: "test".into(),
            protocol: "openai-chat-completions".into(),
            base_url: "https://example.test".into(),
            api_key_env: None,
            model: "test-model".into(),
            max_output_tokens: None,
            reasoning_effort: None,
        };
        let prompt = system_prompt(&profile, "/tmp", 100, &mounts);
        assert!(prompt.contains("Filesystem root"), "{prompt}");
        assert!(prompt.contains("do not retry"), "{prompt}");
    }

    #[test]
    fn prompt_starts_with_main_instructions_and_uses_ai_identity() {
        let mounts = invocation_mounts(AccessMode::Full, "/tmp", Vec::new()).unwrap();
        let profile = config::ResolvedProfile {
            name: "test".into(),
            protocol: "openai-chat-completions".into(),
            base_url: "https://example.test".into(),
            api_key_env: None,
            model: "deepseek-v4-flash".into(),
            max_output_tokens: None,
            reasoning_effort: None,
        };
        let prompt = system_prompt(&profile, "/tmp", 100, &mounts);
        assert!(prompt.starts_with("<main_instructions>"), "{prompt}");
        assert!(prompt.contains("You are an AI assistant."), "{prompt}");
        assert!(prompt.contains("model_id: deepseek-v4-flash"), "{prompt}");
        assert!(
            prompt.find("<main_instructions>").unwrap() < prompt.find("<environment>").unwrap()
        );
        assert!(
            prompt.find("<environment>").unwrap() < prompt.find("<common_principles>").unwrap()
        );
        assert!(
            prompt.find("<common_principles>").unwrap() < prompt.find("<tool_contract>").unwrap()
        );
        assert!(!prompt.contains("You are deepseek-v4-flash"), "{prompt}");
        assert!(!prompt.contains("maxSteps"), "{prompt}");
        let main_end = prompt.find("</main_instructions>").unwrap();
        let tool_start = prompt.find("<tool_contract>").unwrap();
        let list_rule = prompt.find("host.fs.list(dir)`").unwrap();
        assert!(main_end < tool_start, "{prompt}");
        assert!(tool_start < list_rule, "{prompt}");
        assert!(
            prompt.contains("Make one defensive, task-complete attempt"),
            "{prompt}"
        );
        assert!(
            prompt.contains("A defensive one-pass workflow can combine several host APIs"),
            "{prompt}"
        );
        assert!(prompt.contains("elapsedMs"), "{prompt}");
        assert!(!prompt.contains("The one-turn protocol"), "{prompt}");
        assert!(
            !prompt.contains("Once you have enough evidence"),
            "{prompt}"
        );
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
    fn only_closed_run_fences_are_programs() {
        let Extracted::Run(block) = extract("notes\n```run\nreturn 42\n```") else {
            panic!("expected run")
        };
        assert_eq!(block.code, "return 42\n");
        assert!(matches!(extract("plain answer"), Extracted::NoRun));
        assert!(matches!(extract("```run\nreturn 42"), Extracted::Truncated));
    }

    #[test]
    fn restricted_access_keeps_explicit_mounts_for_the_invocation() {
        let extra = Mount::from_canonical("/outside", std::env::temp_dir(), false).unwrap();
        let mounts = invocation_mounts(AccessMode::ReadOnly, "/tmp", vec![extra]).unwrap();
        assert_eq!(
            mounts.iter().map(Mount::virtual_path).collect::<Vec<_>>(),
            ["/workspace/", "/outside/"]
        );
        let profile = config::ResolvedProfile {
            name: "test".into(),
            protocol: "openai-chat-completions".into(),
            base_url: "https://example.test".into(),
            api_key_env: None,
            model: "test-model".into(),
            max_output_tokens: None,
            reasoning_effort: None,
        };
        let prompt = system_prompt(&profile, "/tmp", 100, &mounts);
        assert!(prompt.contains("/workspace, /outside"), "{prompt}");
        assert!(prompt.contains("--mount"), "{prompt}");
    }

    #[test]
    fn exactly_one_run_block_is_enforced() {
        assert!(matches!(
            extract("```run\nreturn 1\n```\n```run\nreturn 2\n```"),
            Extracted::Multiple
        ));
    }
}
