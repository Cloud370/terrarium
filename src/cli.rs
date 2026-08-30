//! Command-line adapter for the Terrarium library.

use std::io::Write;
use std::path::PathBuf;

use crate::{add_mount, agent, llm, Kernel, Mount};

#[derive(Debug, Default)]
struct RunArgs {
    timeout_ms: Option<u64>,
    mounts: Vec<Mount>,
    contract: bool,
    expression: Option<String>,
    file: Option<PathBuf>,
    read_only: bool,
    full_access: bool,
}

fn parse_run_args(args: &[String]) -> Result<RunArgs, String> {
    let mut parsed = RunArgs::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-e" if i + 1 < args.len() => {
                if parsed.expression.is_some() || parsed.file.is_some() {
                    return Err("run accepts exactly one of -e, a source file, or stdin".into());
                }
                parsed.expression = Some(args[i + 1].clone());
                i += 2;
            }
            "-e" => return Err("-e expects JavaScript source".into()),
            "--timeout-ms" if i + 1 < args.len() => {
                parsed.timeout_ms = Some(
                    args[i + 1]
                        .parse::<u64>()
                        .ok()
                        .filter(|n| *n >= 1)
                        .ok_or_else(|| {
                            "--timeout-ms expects an integer >= 1 (milliseconds)".to_string()
                        })?,
                );
                i += 2;
            }
            "--timeout-ms" => return Err("--timeout-ms expects a value".into()),
            "--contract" => {
                parsed.contract = true;
                i += 1;
            }
            "--read-only" => {
                parsed.read_only = true;
                i += 1;
            }
            "--full-access" => {
                parsed.full_access = true;
                i += 1;
            }
            "--mount" if i + 1 < args.len() => {
                add_mount(&mut parsed.mounts, &args[i + 1])?;
                i += 2;
            }
            "--mount" => return Err("--mount expects /virtual=real[:rw]".into()),
            arg if arg.starts_with("--") => {
                return Err(format!("unknown or incomplete flag: {arg}"))
            }
            arg => {
                if parsed.expression.is_some() || parsed.file.is_some() {
                    return Err("run accepts exactly one of -e, a source file, or stdin".into());
                }
                parsed.file = Some(PathBuf::from(arg));
                i += 1;
            }
        }
    }
    if parsed.read_only && parsed.full_access {
        return Err("--read-only and --full-access are mutually exclusive".into());
    }
    Ok(parsed)
}

async fn run_direct(args: &[String]) -> i32 {
    let parsed = match parse_run_args(args) {
        Ok(v) => v,
        Err(error) => {
            eprintln!("terrarium: {error}");
            eprintln!("usage: terrarium run [-e SOURCE | FILE] [--read-only | --full-access] [--mount /virtual=real[:rw]] [--timeout-ms N]");
            return 2;
        }
    };
    let writable = !parsed.read_only;
    let root = if parsed.full_access {
        Mount::new("/", "/", true)
    } else {
        Mount::new("/workspace", ".", writable)
    };
    let root = match root {
        Ok(mount) => mount,
        Err(error) => {
            eprintln!("terrarium: {error}");
            return 2;
        }
    };
    let mut mounts = vec![root];
    mounts.extend(parsed.mounts);
    let kernel = match Kernel::new(mounts) {
        Ok(v) => v,
        Err(error) => {
            eprintln!("terrarium: {error}");
            return 2;
        }
    };
    if parsed.contract {
        print!("{}", kernel.contract());
        return 0;
    }
    let code = match (parsed.expression, parsed.file) {
        (Some(source), None) => source,
        (None, Some(path)) => match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => {
                eprintln!(
                    "terrarium: cannot read source file {}: {error}",
                    path.display()
                );
                return 2;
            }
        },
        (None, None) => match std::io::read_to_string(std::io::stdin()) {
            Ok(source) => source,
            Err(error) => {
                eprintln!("terrarium: stdin is not valid UTF-8: {error}");
                return 2;
            }
        },
        (Some(_), Some(_)) => unreachable!(),
    };
    let timeout_ms = parsed.timeout_ms.unwrap_or(2_000);
    let outcome = kernel.run(&code, timeout_ms).await;
    let mut stdout = std::io::stdout();
    let _ = writeln!(
        stdout,
        "{}",
        serde_json::json!({
            "ok": outcome.ok,
            "value": outcome.value,
            "stdout": outcome.stdout,
            "error": outcome.error,
            "termination": outcome.termination,
            "timed_out": outcome.timed_out,
            "elapsed_ms": outcome.elapsed_ms,
            "writes": outcome.writes,
            "writes_truncated": outcome.writes_truncated,
            "target": format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            "limits": {"memory":"64MB","stack":"1MB","timeout_ms":timeout_ms},
            "mounts": kernel
                .mounts()
                .iter()
                .map(|m| {
                    if m.virtual_path() == "/" {
                        "/".to_string()
                    } else {
                        m.virtual_path().trim_end_matches('/').to_string()
                    }
                })
                .collect::<Vec<_>>(),
            "llm_usage": llm::usage_json()
        })
    );
    let _ = stdout.flush();
    i32::from(!outcome.ok)
}

pub async fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("run") {
        return run_direct(&args[1..]).await;
    }
    agent::run_cli(args).await
}

#[cfg(test)]
mod tests {
    use super::parse_run_args;

    #[test]
    fn run_inputs_are_mutually_exclusive() {
        assert!(parse_run_args(&["-e".into(), "return 1".into()])
            .unwrap()
            .expression
            .is_some());
        assert!(parse_run_args(&["-e".into(), "return 1".into(), "file.js".into()]).is_err());
        assert!(parse_run_args(&["--timeout-ms".into()]).is_err());
        assert!(parse_run_args(&["--read-only".into(), "--full-access".into()]).is_err());
        let mount_spec = format!("/data={}", std::env::temp_dir().display());
        let parsed = parse_run_args(&[
            "--read-only".into(),
            "--mount".into(),
            mount_spec,
            "-e".into(),
            "return 1".into(),
        ])
        .unwrap();
        assert!(parsed.read_only);
        assert_eq!(parsed.mounts.len(), 1);
    }
}
