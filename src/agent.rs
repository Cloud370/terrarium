//! agent —— the outer loop (D012): conversation state lives OUTSIDE the cage, every run gets a FRESH cage.
//! Dumb by design — transport, extraction, sandbox invocation, truncation, stats. No policy: termination is
//! taught by the contract, not enforced here (no default round cap, no convergence forcing; `--max-rounds`
//! is an honest kill switch, not a behavior modification).

use std::time::Instant;
use tokio::sync::watch;

use crate::{eval_js, fs, llm};

const ROLE_TEMPLATE: &str = include_str!("MAIN.md");
const FEEDBACK_CAP: usize = 12 * 1024; // result JSON re-entering the conversation (provisional, demo parity)
const RUN_TIMEOUT_DEFAULT_MS: u64 = 2_000;
const RUN_TIMEOUT_CAP_MS: u64 = 300_000;

pub struct Options {
    pub mounts: Vec<fs::Mount>,
    pub max_rounds: Option<u64>,
    pub run_timeout_ms: u64,
}

pub struct RunBlock {
    pub code: String,
    pub timeout_ms: Option<u64>,
}

/// Earliest fenced block whose tag is one of `tags`. Skips non-matching fences entirely (regex-alternation
/// semantics); an opening without a closing backtracks past it, same as the prelude regex.
fn fence_body(reply: &str, tags: &[&str]) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = reply[from..].find("```") {
        let after = from + rel + 3;
        let Some(eol) = reply[after..].find('\n') else {
            from = after;
            continue;
        };
        let line_end = after + eol;
        if tags.contains(&reply[after..line_end].trim()) {
            let body_start = line_end + 1;
            if let Some(close) = reply[body_start..].find("```") {
                return Some(
                    reply[body_start..body_start + close]
                        .trim_start_matches('\n')
                        .to_string(),
                );
            }
        }
        from = after;
    }
    None
}

/// First tagless fence only — an unclosed one ends the search (prelude parity: later bare fences are never considered)
fn first_bare_fence(reply: &str) -> Option<String> {
    let mut from = 0;
    while let Some(rel) = reply[from..].find("```") {
        let after = from + rel + 3;
        let Some(eol) = reply[after..].find('\n') else {
            from = after;
            continue;
        };
        let line_end = after + eol;
        if reply[after..line_end].trim().is_empty() {
            let body_start = line_end + 1;
            return reply[body_start..].find("```").map(|close| {
                reply[body_start..body_start + close]
                    .trim_start_matches('\n')
                    .to_string()
            });
        }
        from = after;
    }
    None
}

/// `<run>...</run>`, case-insensitive
fn run_tag_body(reply: &str) -> Option<String> {
    let b = reply.as_bytes();
    let mut i = 0;
    while i + 5 <= b.len() {
        if b[i] == b'<' && reply[i..i + 5].eq_ignore_ascii_case("<run>") {
            let mut j = i + 5;
            while j + 6 <= b.len() {
                if b[j] == b'<' && reply[j..j + 6].eq_ignore_ascii_case("</run>") {
                    return Some(reply[i + 5..j].to_string());
                }
                j += 1;
            }
            return None;
        }
        i += 1;
    }
    None
}

/// JS smell, literal-for-literal from prelude's `__extractRun`
fn smells_like_js(code: &str) -> bool {
    ["host.", "return", "await ", "const ", "let ", "function "]
        .iter()
        .any(|k| code.contains(k))
}

/// Block-header budget channel (fences carry no argument slot; this restores demo's `timeout_ms` tool arg):
/// first non-empty line `// timeout-ms: N`. Malformed → None (the default applies).
fn parse_timeout_directive(code: &str) -> Option<u64> {
    let first = code.lines().find(|l| !l.trim().is_empty())?;
    first
        .trim()
        .strip_prefix("// timeout-ms:")?
        .trim()
        .parse::<u64>()
        .ok()
}

/// Rust port of prelude `__extractRun` — the two must stay behavior-aligned (protocol is defined once, implemented twice).
pub fn extract_run(reply: &str) -> Option<RunBlock> {
    let code = fence_body(reply, &["run", "js", "javascript"])
        .or_else(|| run_tag_body(reply))
        .or_else(|| first_bare_fence(reply).filter(|c| smells_like_js(c)))?;
    let timeout_ms = parse_timeout_directive(&code);
    Some(RunBlock { code, timeout_ms })
}

/// A committed final answer: the first non-blank line starts with `FINAL:` — the positive stop signal.
/// Nothing after it is extracted or run; absence of a run block remains the fallback.
fn is_final_prefixed(reply: &str) -> bool {
    reply
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim_start().starts_with("FINAL:"))
        .unwrap_or(false)
}

/// Consulted ONLY when extraction found nothing: an opening ```run/js/javascript fence or <run> tag that
/// produced no block was left unclosed — almost surely output truncation mid-program. Feeding it back as
/// an error beats ending the session on a half-written "final answer". (A closed fence always extracts,
/// so a bare `contains` can't false-positive here.)
fn unclosed_block_opener(reply: &str) -> bool {
    ["```run", "```js", "```javascript"]
        .iter()
        .any(|t| reply.contains(t))
        || reply.to_ascii_lowercase().contains("<run>")
}

fn truncate_utf8(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

/// demo parity: fall back to ./.env for the key when the environment doesn't carry it (host-side only)
fn load_dotenv() {
    if std::env::var("DEEPSEEK_API_KEY")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return;
    }
    let Ok(text) = std::fs::read_to_string(".env") else {
        return;
    };
    for line in text.lines() {
        if let Some(v) = line.trim().strip_prefix("DEEPSEEK_API_KEY=").map(str::trim) {
            if !v.is_empty() {
                std::env::set_var("DEEPSEEK_API_KEY", v);
            }
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage: terrarium agent <task-file | task text> [--mount /virt=real[:rw]]... [--max-rounds N] [--run-timeout-ms N]\n\
         exit codes: 0 = answered, 1 = transport/internal failure, 2 = max-rounds budget exhausted"
    );
}

pub async fn run_cli(args: &[String]) -> i32 {
    let mut mounts: Vec<fs::Mount> = Vec::new();
    let mut max_rounds: Option<u64> = None;
    let mut run_timeout_ms: u64 = RUN_TIMEOUT_DEFAULT_MS;
    let mut task_arg: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mount" if i + 1 < args.len() => {
                crate::add_mount(&mut mounts, &args[i + 1]);
                i += 2;
            }
            "--max-rounds" if i + 1 < args.len() => match args[i + 1].parse::<u64>() {
                Ok(n) if n >= 1 => max_rounds = Some(n),
                _ => {
                    eprintln!("--max-rounds expects an integer >= 1");
                    return 1;
                }
            },
            "--run-timeout-ms" if i + 1 < args.len() => match args[i + 1].parse::<u64>() {
                Ok(n) if n >= 1 => run_timeout_ms = n,
                _ => {
                    eprintln!("--run-timeout-ms expects an integer >= 1");
                    return 1;
                }
            },
            "--help" | "-h" => {
                print_usage();
                return 0;
            }
            a if a.starts_with("--") => {
                eprintln!("unknown or incomplete flag: {a}");
                print_usage();
                return 1;
            }
            a => {
                if task_arg.is_some() {
                    eprintln!("expected exactly one task (file or text)");
                    return 1;
                }
                task_arg = Some(a.to_string());
                i += 1;
            }
        }
    }
    let Some(task_src) = task_arg else {
        print_usage();
        return 1;
    };
    let task = std::fs::read_to_string(&task_src).unwrap_or(task_src); // unreadable as a file → literal task text
    load_dotenv();

    let opts = Options {
        mounts,
        max_rounds,
        run_timeout_ms,
    };
    run(&task, &opts).await
}

async fn run(task: &str, opts: &Options) -> i32 {
    // numbers in the role layer are filled from the same constants the loop enforces (single source)
    let role = ROLE_TEMPLATE
        .replace("{{RUN_DEFAULT_MS}}", &opts.run_timeout_ms.to_string())
        .replace("{{RUN_CAP_MS}}", &RUN_TIMEOUT_CAP_MS.to_string())
        .replace("{{MODEL}}", &llm::model_name());
    let system = format!(
        "{}\n\n# Main agent\n\n{}",
        crate::contract(&opts.mounts),
        role
    );
    let mut messages: Vec<serde_json::Value> = vec![
        serde_json::json!({ "role": "system", "content": system }),
        serde_json::json!({ "role": "user", "content": task }),
    ];
    let t_all = Instant::now();
    let (mut runs, mut round) = (0u64, 0u64);
    let mut feedback_bytes = 0usize;
    let (mut outer_hit, mut outer_miss) = (0u64, 0u64);

    loop {
        round += 1;
        if let Some(max) = opts.max_rounds {
            if round > max {
                eprintln!("[agent] max rounds ({max}) exhausted — honest abort, no forcing");
                if let Some(last) = messages.iter().rev().find(|m| m["role"] == "assistant") {
                    println!("{}", last["content"].as_str().unwrap_or(""));
                }
                eprintln!(
                    "[stats] rounds={round} runs={runs} feedback={:.1}KB wall={:.1}s",
                    feedback_bytes as f64 / 1024.0,
                    t_all.elapsed().as_secs_f64()
                );
                return 2;
            }
        }

        let (_, h0, m0, _) = llm::usage_snapshot();
        let t0 = Instant::now();
        let reply = match llm::complete(messages.clone()).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[agent] llm transport failed after retries: {e}");
                return 1;
            }
        };
        let (_, h1, m1, _) = llm::usage_snapshot();
        outer_hit += h1 - h0;
        outer_miss += m1 - m0;
        messages.push(serde_json::json!({ "role": "assistant", "content": reply }));

        // stop protocol: FINAL: prefix is the committed signal (nothing after it is extracted or run);
        // an unclosed block opener past failed extraction is truncation — an error to feed back, not a final answer
        let committed_final = is_final_prefixed(&reply);
        let block = match if committed_final {
            None
        } else {
            extract_run(&reply)
        } {
            Some(b) => Some(b),
            None if !committed_final && unclosed_block_opener(&reply) => {
                eprintln!("[round {round}] run block unclosed (output truncated?) — error fed back, not final");
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": "run block error: your block opened but never closed (output truncated?) — resend the complete ```run program"
                }));
                continue;
            }
            None => None,
        };
        let Some(block) = block else {
            eprintln!(
                "[round {round}] wall={}ms cache hit={} miss={} — final answer",
                t0.elapsed().as_millis(),
                h1 - h0,
                m1 - m0
            );
            let text = reply
                .trim_start()
                .strip_prefix("FINAL:")
                .map(str::trim_start)
                .unwrap_or(reply.trim_start());
            println!("{text}");
            let (calls, gh, gm, out_tok) = llm::usage_snapshot();
            let total = outer_hit + outer_miss;
            eprintln!(
                "[stats] rounds={round} runs={runs} feedback={:.1}KB wall={:.1}s outer_cache hit={outer_hit} miss={outer_miss} ({}%) llm_usage calls={calls} hit={gh} miss={gm} out={out_tok}",
                feedback_bytes as f64 / 1024.0,
                t_all.elapsed().as_secs_f64(),
                    if total > 0 { outer_hit.checked_mul(100).and_then(|n| n.checked_div(total)).unwrap_or(0) } else { 0 }
            );
            return 0;
        };
        eprintln!(
            "[round {round}] wall={}ms cache hit={} miss={} — run block ({} bytes)",
            t0.elapsed().as_millis(),
            h1 - h0,
            m1 - m0,
            block.code.len()
        );

        let timeout_ms = block
            .timeout_ms
            .unwrap_or(opts.run_timeout_ms)
            .min(RUN_TIMEOUT_CAP_MS);
        let (cancel_tx, _rx) = watch::channel(false);
        let out = eval_js(&block.code, timeout_ms, &opts.mounts, cancel_tx).await;
        runs += 1;
        eprintln!(
            "  run #{runs}: {}ms ok={}{}{}",
            out.elapsed_ms,
            out.ok,
            if out.timed_out { " TIMEOUT" } else { "" },
            out.error
                .as_ref()
                .map(|e| format!(
                    " err={}",
                    e.lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(80)
                        .collect::<String>()
                ))
                .unwrap_or_default()
        );

        let payload = serde_json::json!({
            "round": round, // the one session fact injected per turn: self-knowledge for pacing, not a countdown
            "ok": out.ok, "result": out.result, "stdout": out.stdout,
            "error": out.error, "timed_out": out.timed_out, "elapsed_ms": out.elapsed_ms,
        })
        .to_string();
        let s = truncate_utf8(&payload, FEEDBACK_CAP);
        feedback_bytes += s.len();
        messages.push(serde_json::json!({ "role": "user", "content": s }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_fences_run_js_javascript() {
        for tag in ["run", "js", "javascript"] {
            let reply = format!("prose\n```{tag}\nreturn 1+1\n```\nafter");
            let b = extract_run(&reply).unwrap();
            assert_eq!(b.code, "return 1+1\n"); // trailing newline kept (prelude lazy-capture parity)
        }
    }

    #[test]
    fn tag_allows_trailing_spaces_and_blank_first_line() {
        let b = extract_run("```run  \n\nreturn 42\n```").unwrap();
        assert_eq!(b.code, "return 42\n"); // leading newline stripped (prelude \s*\n parity)
    }

    #[test]
    fn earliest_tagged_fence_wins_and_json_is_skipped() {
        let reply = "look:\n```json\n{\"a\": 1}\n```\nthen:\n```run\nreturn host.fs\n```";
        let b = extract_run(reply).unwrap();
        assert_eq!(b.code, "return host.fs\n");
    }

    #[test]
    fn run_tag_case_insensitive() {
        let b = extract_run("<RUN>\nreturn 3\n</RUN>").unwrap();
        assert_eq!(b.code, "\nreturn 3\n");
    }

    #[test]
    fn bare_fence_needs_js_smell() {
        assert!(extract_run("```\nreturn 1\n```").is_some());
        assert!(extract_run("```\njust prose here\n```").is_none());
        // first bare fence only (prelude parity): non-JS first blocks later candidates
        assert!(extract_run("```\nno smell\n```\n```\nreturn 1\n```").is_none());
    }

    #[test]
    fn unclosed_tagged_fence_falls_through() {
        let b = extract_run("```run\nnever closes\n<run>\nreturn 9\n</run>").unwrap();
        assert_eq!(b.code.trim(), "return 9");
    }

    #[test]
    fn no_block_is_final_answer() {
        assert!(extract_run("plain final report, no fences").is_none());
        assert!(extract_run("").is_none());
    }

    #[test]
    fn timeout_directive_parsed_from_first_nonempty_line() {
        let b = extract_run("```run\n// timeout-ms: 60000\nreturn 1\n```").unwrap();
        assert_eq!(b.timeout_ms, Some(60_000));
        let b = extract_run("```run\n\n  // timeout-ms: 240000\nreturn 1\n```").unwrap();
        assert_eq!(b.timeout_ms, Some(240_000));
    }

    #[test]
    fn malformed_directive_ignored() {
        assert_eq!(
            extract_run("```run\n// timeout-ms: soon\nreturn 1\n```")
                .unwrap()
                .timeout_ms,
            None
        );
        assert_eq!(
            extract_run("```run\nreturn 1\n```").unwrap().timeout_ms,
            None
        );
    }

    #[test]
    fn final_prefix_detected_on_first_nonblank_line_only() {
        assert!(is_final_prefixed("FINAL: 42"));
        assert!(is_final_prefixed("\n  \nFINAL: strict JSON"));
        assert!(!is_final_prefixed("the answer is FINAL: soon")); // mid-line doesn't count
        assert!(!is_final_prefixed("```run\nreturn 1\n```"));
        assert!(!is_final_prefixed(""));
    }

    #[test]
    fn unclosed_opener_spotted_only_post_extraction() {
        // true positives: extraction already failed for these (unclosed) openers
        assert!(unclosed_block_opener("checking:\n```run\nreturn par"));
        assert!(unclosed_block_opener("<RUN>\nreturn 3"));
        assert!(unclosed_block_opener("```js\nconst x"));
        // a closed fence always extracts, so the loop never consults the guard for it — the plain
        // contains() here is only sound under that call order (documented on the fn)
        assert!(!unclosed_block_opener("plain final answer"));
        assert!(!unclosed_block_opener(""));
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        let s = "中文".repeat(100); // 3-byte chars
        let t = truncate_utf8(&s, 10); // 10 is mid-char
        assert!(t.ends_with("…[truncated]"));
        assert!(t.starts_with('中'));
        assert_eq!(truncate_utf8("short", 100), "short");
    }
}
