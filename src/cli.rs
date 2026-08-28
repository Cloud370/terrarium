//! Command-line adapter for the Terrarium library.

use std::io::Write;

use crate::{add_mount, agent, llm, Kernel, Mount};

#[derive(Debug, Default)]
struct RunArgs {
    timeout_ms: Option<u64>,
    mounts: Vec<Mount>,
    contract: bool,
    code: String,
}

fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let mut parsed = RunArgs::default();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--timeout-ms" if i + 1 < args.len() => {
                match args[i + 1].parse::<u64>() {
                    Ok(n) if n >= 1 => parsed.timeout_ms = Some(n),
                    _ => return Err(format!("{arg} expects an integer >= 1 (milliseconds)")),
                }
                i += 2;
            }
            "--contract" => {
                parsed.contract = true;
                i += 1;
            }
            "--mount" if i + 1 < args.len() => {
                add_mount(&mut parsed.mounts, &args[i + 1])?;
                i += 2;
            }
            arg if arg.starts_with("--") => {
                return Err(format!("unknown or incomplete flag: {arg}"));
            }
            arg => {
                if !parsed.code.is_empty() {
                    parsed.code.push(' ');
                }
                parsed.code.push_str(arg);
                i += 1;
            }
        }
    }
    Ok(parsed)
}

pub async fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("agent") {
        return agent::run_cli(&args[1..]).await;
    }

    let parsed = match parse_run_args(args) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("terrarium: {error}");
            eprintln!(
                "usage: terrarium [--timeout-ms N] [--mount /virt=real[:rw]]... [--contract] [code]"
            );
            return 2;
        }
    };

    let kernel = match Kernel::new(parsed.mounts) {
        Ok(kernel) => kernel,
        Err(error) => {
            eprintln!("terrarium: {error}");
            return 2;
        }
    };
    if parsed.contract {
        print!("{}", kernel.contract());
        return 0;
    }

    let timeout_ms = parsed.timeout_ms.unwrap_or(2_000);
    let mut code = parsed.code;
    if code.is_empty() {
        code = match std::io::read_to_string(std::io::stdin()) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("terrarium: stdin is not valid UTF-8: {error}");
                return 2;
            }
        };
    }

    let outcome = kernel.run(&code, timeout_ms).await;
    let mut stdout = std::io::stdout();
    let _ = writeln!(
        stdout,
        "{}",
        serde_json::json!({
            "ok": outcome.ok,
            "value": outcome.value,
            "answer": outcome.answer,
            "stdout": outcome.stdout,
            "error": outcome.error,
            "termination": outcome.termination,
            "timed_out": outcome.timed_out,
            "elapsed_ms": outcome.elapsed_ms,
            "target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            "limits": { "memory": "64MB", "stack": "1MB", "timeout_ms": timeout_ms },
            "mounts": kernel
                .mounts()
                .iter()
                .map(|mount| mount.virtual_path().trim_end_matches('/').to_string())
                .collect::<Vec<_>>(),
            "llm_usage": llm::usage_json(),
        })
    );
    let _ = stdout.flush();
    i32::from(!outcome.ok)
}

#[cfg(test)]
mod tests {
    use super::parse_run_args;

    #[test]
    fn run_mode_flags_are_guarded_and_order_insensitive() {
        assert!(parse_run_args(&["--timeout-ms".into()])
            .unwrap_err()
            .contains("incomplete flag"));
        assert!(parse_run_args(&["--timeout".into(), "50".into()])
            .unwrap_err()
            .contains("unknown or incomplete flag: --timeout"));
        assert!(parse_run_args(&["--timeout-ms".into(), "abc".into()])
            .unwrap_err()
            .contains("milliseconds"));
        let parsed = parse_run_args(&["--timeout-ms".into(), "50".into()]).unwrap();
        assert_eq!(parsed.timeout_ms, Some(50));
        assert!(parse_run_args(&["--wat".into()]).is_err());
        assert!(parse_run_args(&[]).unwrap().code.is_empty());

        let root = std::env::temp_dir().join(format!("terrarium-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let parsed = parse_run_args(&[
            "--contract".into(),
            "--mount".into(),
            format!("/x={}", root.display()),
        ])
        .unwrap();
        assert!(parsed.contract && parsed.mounts.len() == 1);
    }
}
