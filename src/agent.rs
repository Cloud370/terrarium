//! The outer agent loop. A model turn is a program; `host.agent.answer` commits the session.

use std::time::Instant;
use tokio::sync::watch;

use crate::{eval_js, fs, llm, truncate_utf8, ErrorKind, Outcome, MAX_TIMEOUT_MS};

const ROLE_TEMPLATE: &str = include_str!("prompts/main.md");
const RUN_TIMEOUT_DEFAULT_MS: u64 = 10_000;
const FEEDBACK_CAP: usize = 12 * 1024;
const DEFAULT_MAX_ROUNDS: u64 = 32;

pub struct Options {
    pub mounts: Vec<fs::Mount>,
    pub max_rounds: u64,
    pub run_timeout_ms: u64,
}

pub struct RunProgram {
    pub code: String,
    pub timeout_ms: Option<u64>,
}

enum Hit {
    Body(String),
    Unclosed,
}

fn scan_run_fences(reply: &str) -> Option<Hit> {
    let mut from = 0;
    while let Some(rel) = reply[from..].find("```") {
        let after = from + rel + 3;
        let Some(eol) = reply[after..].find('\n') else {
            from = after;
            continue;
        };
        let line_end = after + eol;
        if reply[after..line_end].trim() == "run" {
            let body_start = line_end + 1;
            return match reply[body_start..].find("```") {
                Some(close) => Some(Hit::Body(
                    reply[body_start..body_start + close]
                        .trim_start_matches('\n')
                        .to_string(),
                )),
                None => Some(Hit::Unclosed),
            };
        }
        from = after;
    }
    None
}

fn parse_timeout_directive(code: &str) -> Option<u64> {
    let first = code.lines().find(|line| !line.trim().is_empty())?;
    first
        .trim()
        .strip_prefix("// timeout-ms:")?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|value| *value >= 1)
}

pub(crate) enum Extracted {
    Run(RunProgram),
    NoRun,
    Truncated,
}

pub(crate) fn extract(reply: &str) -> Extracted {
    match scan_run_fences(reply) {
        Some(Hit::Body(code)) => Extracted::Run(RunProgram {
            timeout_ms: parse_timeout_directive(&code),
            code,
        }),
        Some(Hit::Unclosed) => Extracted::Truncated,
        None => Extracted::NoRun,
    }
}

fn print_usage() {
    eprintln!(
        "usage: terrarium agent <task-file | task text> [--mount /virt=real[:rw]]... [--max-rounds N] [--run-timeout-ms N]\n\
         exit codes: 0 = answered, 1 = transport/internal failure, 2 = round budget exhausted"
    );
}

#[derive(Debug)]
struct ParsedArgs {
    mounts: Vec<fs::Mount>,
    max_rounds: u64,
    run_timeout_ms: u64,
    task: String,
}

fn parse_agent_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut mounts = Vec::new();
    let mut max_rounds = DEFAULT_MAX_ROUNDS;
    let mut run_timeout_ms = RUN_TIMEOUT_DEFAULT_MS;
    let mut task = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mount" if i + 1 < args.len() => {
                crate::add_mount(&mut mounts, &args[i + 1])?;
                i += 2;
            }
            "--max-rounds" if i + 1 < args.len() => match args[i + 1].parse::<u64>() {
                Ok(value) if value >= 1 => {
                    max_rounds = value;
                    i += 2;
                }
                _ => return Err("--max-rounds expects an integer >= 1".into()),
            },
            "--run-timeout-ms" if i + 1 < args.len() => match args[i + 1].parse::<u64>() {
                Ok(value) if (1..=MAX_TIMEOUT_MS).contains(&value) => {
                    run_timeout_ms = value;
                    i += 2;
                }
                _ => return Err(format!("--run-timeout-ms expects 1..={MAX_TIMEOUT_MS}")),
            },
            arg if arg.starts_with("--") => {
                return Err(format!("unknown or incomplete flag: {arg}"))
            }
            arg => {
                if task.is_some() {
                    return Err("expected exactly one task (file or text)".into());
                }
                task = Some(arg.to_string());
                i += 1;
            }
        }
    }
    Ok(ParsedArgs {
        mounts,
        max_rounds,
        run_timeout_ms,
        task: task.ok_or_else(String::new)?,
    })
}

pub async fn run_cli(args: &[String]) -> i32 {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_usage();
        return 0;
    }
    let parsed = match parse_agent_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            if !error.is_empty() {
                eprintln!("{error}");
            }
            print_usage();
            return 1;
        }
    };
    let task_path = std::path::Path::new(&parsed.task);
    let task = if task_path.is_file() {
        match std::fs::read_to_string(task_path) {
            Ok(task) => task,
            Err(error) => {
                eprintln!(
                    "[agent] cannot read task file {}: {error}",
                    task_path.display()
                );
                return 1;
            }
        }
    } else {
        parsed.task.clone()
    };
    let opts = Options {
        mounts: parsed.mounts,
        max_rounds: parsed.max_rounds,
        run_timeout_ms: parsed.run_timeout_ms,
    };
    run(&task, &opts).await
}

fn feedback(round: u64, out: &Outcome) -> String {
    let payload = serde_json::json!({
        "round": round,
        "ok": out.ok,
        "value": out.value,
        "stdout": out.stdout,
        "error": out.error,
        "termination": out.termination,
        "elapsed_ms": out.elapsed_ms,
    });
    let full = payload.to_string();
    if full.len() <= FEEDBACK_CAP {
        return full;
    }
    serde_json::json!({
        "round": round,
        "ok": false,
        "value": null,
        "stdout": truncate_utf8(&out.stdout, 4096),
        "error": {"kind": ErrorKind::Protocol, "message": "run result exceeded the feedback limit"},
        "termination": "failed",
        "elapsed_ms": out.elapsed_ms,
        "truncated": true,
    })
    .to_string()
}

async fn run(task: &str, opts: &Options) -> i32 {
    let role = ROLE_TEMPLATE
        .replace("{{RUN_DEFAULT_MS}}", &opts.run_timeout_ms.to_string())
        .replace("{{RUN_CAP_MS}}", &MAX_TIMEOUT_MS.to_string())
        .replace("{{MODEL}}", &llm::model_name())
        .replace("{{MAX_ROUNDS}}", &opts.max_rounds.to_string());
    let system = format!(
        "{}\n\n# Main agent\n\n{}",
        crate::contract(&opts.mounts),
        role
    );
    let mut messages = vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": task}),
    ];
    let started = Instant::now();
    let mut runs = 0u64;
    let mut feedback_bytes = 0usize;

    for round in 1..=opts.max_rounds {
        let reply = match llm::complete(messages.clone()).await {
            Ok(reply) => reply,
            Err(error) => {
                eprintln!("[agent] llm transport failed: {error}");
                return 1;
            }
        };
        messages.push(serde_json::json!({"role": "assistant", "content": reply}));
        let block = match extract(
            messages.last().expect("assistant message")["content"]
                .as_str()
                .unwrap_or_default(),
        ) {
            Extracted::Run(block) => block,
            Extracted::Truncated => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": "protocol error: the run block was not closed; send one complete ```run program"
                }));
                continue;
            }
            Extracted::NoRun => {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": "protocol error: submit one complete ```run program; call host.agent.answer(text) from the program when the task is complete"
                }));
                continue;
            }
        };

        let timeout_ms = block
            .timeout_ms
            .unwrap_or(opts.run_timeout_ms)
            .min(MAX_TIMEOUT_MS);
        if std::env::var("TERRARIUM_LOG_RUNS").as_deref() == Ok("1") {
            eprintln!("[run code]\n{}\n[/run code]", block.code);
        }
        let (cancel_tx, _cancel_rx) = watch::channel(false);
        let out = eval_js(&block.code, timeout_ms, &opts.mounts, cancel_tx).await;
        runs += 1;
        eprintln!(
            "[round {round}] run #{runs}: {}ms termination={:?}{}{}",
            out.elapsed_ms,
            out.termination,
            if out.timed_out { " TIMEOUT" } else { "" },
            out.error
                .as_ref()
                .map(|error| format!(" err={}", error.message.lines().next().unwrap_or("")))
                .unwrap_or_default()
        );

        if let Some(answer) = out.answer.as_deref() {
            println!("{answer}");
            eprintln!(
                "[stats] rounds={round} runs={runs} feedback={:.1}KB wall={:.1}s",
                feedback_bytes as f64 / 1024.0,
                started.elapsed().as_secs_f64()
            );
            return 0;
        }
        let result = feedback(round, &out);
        feedback_bytes += result.len();
        messages.push(serde_json::json!({"role": "user", "content": result}));
    }

    eprintln!("[agent] round budget ({}) exhausted", opts.max_rounds);
    eprintln!(
        "[stats] rounds={} runs={} feedback={:.1}KB wall={:.1}s",
        opts.max_rounds,
        runs,
        feedback_bytes as f64 / 1024.0,
        started.elapsed().as_secs_f64()
    );
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_closed_run_fences_are_programs() {
        let Extracted::Run(block) = extract("notes\n```run\nreturn 42\n```") else {
            panic!("expected a run block")
        };
        assert_eq!(block.code, "return 42\n");
        assert!(matches!(extract("plain answer"), Extracted::NoRun));
        assert!(matches!(extract("```run\nreturn 42"), Extracted::Truncated));
        assert!(matches!(
            extract("```javascript\nreturn 42\n```"),
            Extracted::NoRun
        ));
    }

    #[test]
    fn timeout_directive_is_bounded_and_explicit() {
        assert_eq!(
            parse_timeout_directive("\n // timeout-ms: 60000\nreturn 1"),
            Some(60_000)
        );
        assert_eq!(parse_timeout_directive("// timeout-ms: 0\nreturn 1"), None);
        assert_eq!(
            parse_timeout_directive("// timeout-ms: soon\nreturn 1"),
            None
        );
    }

    #[test]
    fn agent_args_have_a_finite_default_and_validate_timeout() {
        let parsed = parse_agent_args(&["task".into()]).unwrap();
        assert_eq!(parsed.max_rounds, DEFAULT_MAX_ROUNDS);
        assert!(parse_agent_args(&["task".into(), "--run-timeout-ms".into(), "0".into()]).is_err());
        assert!(parse_agent_args(&[
            "task".into(),
            "--run-timeout-ms".into(),
            (MAX_TIMEOUT_MS + 1).to_string()
        ])
        .is_err());
    }
}
