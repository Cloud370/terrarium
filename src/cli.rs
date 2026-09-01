//! Command-line adapter for the Terrarium library.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;

use crate::{
    agent,
    auth::{Authorizer, Decision, ResolvedAccessRequest},
    fs::{RunFilesystemAuthority, WriteScope},
    llm, Kernel,
};

/// One terminal decision prompt. An invocation without an interactive stdin — a pipe, CI
/// job, or background run — reports `Unavailable` instead of guessing; EOF while prompting
/// cancels rather than approving.
struct TerminalAuthorizer;

impl Authorizer for TerminalAuthorizer {
    fn decide(&self, request: &ResolvedAccessRequest) -> Decision {
        if !std::io::stdin().is_terminal() {
            return Decision::Unavailable;
        }
        let mut out = std::io::stdout();
        let _ = writeln!(out, "Terrarium requests authorization for this run");
        if !request.reason.trim().is_empty() {
            let _ = writeln!(out, "Reason: {}", request.reason);
        }
        for target in &request.targets {
            let note = if target.parents_missing {
                " (new file; missing parent directories will be created)"
            } else {
                ""
            };
            let _ = writeln!(out, "  write {}{}", target.display, note);
        }
        // "what you read is what runs": the exact argv with the executable already
        // resolved and the working directory spelled out
        for command in &request.commands {
            let _ = writeln!(out, "  run   {}", command.display);
        }
        let asks = match (request.targets.is_empty(), request.commands.is_empty()) {
            (false, true) => "Allow these writes?",
            (true, false) => "Allow these commands?",
            _ => "Allow these writes and commands?",
        };
        let _ = write!(out, "{asks} [y/N] ");
        let _ = out.flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => Decision::Cancel,
            Ok(_) => match line.trim().to_lowercase().as_str() {
                "y" | "yes" => Decision::Allow,
                _ => Decision::Deny,
            },
            Err(_) => Decision::Cancel,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct RunArgs {
    timeout_ms: Option<u64>,
    allow_write: Vec<String>,
    allow_exec: Vec<String>,
    offline: bool,
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
            "--allow-write" if i + 1 < args.len() => {
                parsed.allow_write.push(args[i + 1].clone());
                i += 2;
            }
            "--allow-write" => {
                return Err("--allow-write expects an absolute DIR or FILE path".into())
            }
            "--allow-exec" if i + 1 < args.len() => {
                parsed.allow_exec.push(args[i + 1].clone());
                i += 2;
            }
            "--allow-exec" => {
                return Err("--allow-exec expects an executable NAME or absolute path".into())
            }
            "--offline" => {
                parsed.offline = true;
                i += 1;
            }
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
    if (parsed.read_only || parsed.full_access) && !parsed.allow_write.is_empty() {
        return Err(
            "--allow-write is valid only in planned-write mode; it cannot be combined with \
             --read-only or --full-access"
                .into(),
        );
    }
    if (parsed.read_only || parsed.full_access) && !parsed.allow_exec.is_empty() {
        return Err(
            "--allow-exec is valid only in planned-write mode; it cannot be combined with \
             --read-only or --full-access"
                .into(),
        );
    }
    Ok(parsed)
}

/// Direct run is read-only by default; `--full-access` is the explicit trusted path and
/// `--allow-write` scopes or `--allow-exec` grants switch the invocation to planned-write
/// without a model access block.
fn direct_run_authority(
    parsed: &RunArgs,
) -> Result<(RunFilesystemAuthority, crate::ProcAuthority), String> {
    if parsed.full_access {
        return Ok((
            RunFilesystemAuthority::FullAccess,
            crate::ProcAuthority::Unrestricted,
        ));
    }
    if parsed.allow_write.is_empty() && parsed.allow_exec.is_empty() {
        return Ok((
            RunFilesystemAuthority::ReadOnly,
            crate::ProcAuthority::Denied,
        ));
    }
    // `--allow-exec` alone implies planned-write: writes stay denied (no scopes, no
    // model access block in direct mode) but the granted commands run — a grant
    // must never be silently ignored.
    let mut scopes = Vec::with_capacity(parsed.allow_write.len());
    for spec in &parsed.allow_write {
        scopes.push(WriteScope::from_operator_spec(spec)?);
    }
    let mut grants = Vec::with_capacity(parsed.allow_exec.len());
    for name in &parsed.allow_exec {
        grants.push(crate::auth::operator_exec_grant(name)?);
    }
    Ok((
        RunFilesystemAuthority::Scoped(scopes),
        crate::ProcAuthority::Allowed(crate::CommandSet {
            grants,
            records: Vec::new(),
        }),
    ))
}

async fn run_direct(args: &[String]) -> i32 {
    let parsed = match parse_run_args(args) {
        Ok(v) => v,
        Err(error) => {
            eprintln!("terrarium: {error}");
            eprintln!("usage: terrarium run [-e SOURCE | FILE] [--read-only | --full-access | --allow-write PATH]... [--allow-exec NAME]... [--offline] [--timeout-ms N]");
            return 2;
        }
    };
    let (authority, proc_authority) = match direct_run_authority(&parsed) {
        Ok(v) => v,
        Err(error) => {
            eprintln!("terrarium: {error}");
            return 2;
        }
    };
    let mut kernel = Kernel::new(authority).with_proc(proc_authority);
    if parsed.offline {
        kernel = kernel.offline();
    }
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
            "filesystem": {
                "mode": kernel.authority().mode().as_str(),
                "writeScopes": match kernel.authority() {
                    RunFilesystemAuthority::Scoped(scopes) =>
                        scopes.iter().map(WriteScope::display).collect::<Vec<_>>(),
                    _ => Vec::new(),
                },
            },
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
    agent::run_cli(args, &TerminalAuthorizer).await
}

#[cfg(test)]
mod tests {
    use super::{direct_run_authority, parse_run_args, RunArgs};
    use crate::fs::RunFilesystemAuthority;

    fn run_args(mut base: RunArgs, read_only: bool, full_access: bool) -> RunArgs {
        base.read_only = read_only;
        base.full_access = full_access;
        base
    }

    #[test]
    fn run_inputs_are_mutually_exclusive() {
        assert!(parse_run_args(&["-e".into(), "return 1".into()])
            .unwrap()
            .expression
            .is_some());
        assert!(parse_run_args(&["-e".into(), "return 1".into(), "file.js".into()]).is_err());
        assert!(parse_run_args(&["--timeout-ms".into()]).is_err());
        assert!(parse_run_args(&["--read-only".into(), "--full-access".into()]).is_err());
        assert!(parse_run_args(&["--mount".into(), "/x=/y".into()]).is_err());
    }

    #[test]
    fn allow_write_conflicts_with_fixed_modes() {
        let with_scope = |scope: &str| {
            vec![
                "--allow-write".into(),
                scope.into(),
                "-e".into(),
                "return 1".into(),
            ]
        };
        assert!(parse_run_args(&with_scope("/tmp")).is_ok());
        assert!(parse_run_args(&with_scope("/tmp/definitely/missing")).is_ok());
        assert!(parse_run_args(
            &["--read-only".into()]
                .into_iter()
                .chain(with_scope("/tmp"))
                .collect::<Vec<_>>()
        )
        .is_err());
        assert!(parse_run_args(
            &["--full-access".into()]
                .into_iter()
                .chain(with_scope("/tmp"))
                .collect::<Vec<_>>()
        )
        .is_err());
    }

    #[test]
    fn direct_run_is_read_only_by_default_and_scopes_enable_planned_write() {
        let root = std::env::temp_dir();
        let root_display = root.to_string_lossy().into_owned();
        assert_eq!(
            direct_run_authority(&run_args(RunArgs::default(), false, false))
                .unwrap()
                .0,
            RunFilesystemAuthority::ReadOnly
        );
        assert_eq!(
            direct_run_authority(&run_args(RunArgs::default(), true, false))
                .unwrap()
                .0,
            RunFilesystemAuthority::ReadOnly
        );
        assert_eq!(
            direct_run_authority(&run_args(RunArgs::default(), false, true))
                .unwrap()
                .0,
            RunFilesystemAuthority::FullAccess
        );
        let mut scoped = RunArgs::default();
        scoped.allow_write.push(root_display.clone());
        let authority = direct_run_authority(&scoped).unwrap().0;
        assert_eq!(authority.mode(), crate::fs::FilesystemMode::PlannedWrite);
        // authorize_write takes the resolved identity: temp_dir() is symlinked on macOS
        // (/var -> /private/var) and canonicalize() returns \\?\ forms on Windows
        let probe = root
            .canonicalize()
            .unwrap()
            .join("terrarium-cli-scope-probe.txt");
        assert!(authority
            .authorize_write(&probe.display().to_string(), &probe)
            .is_ok());
        let outside = std::path::PathBuf::from("/definitely/not/covered.txt");
        assert!(authority
            .authorize_write(outside.to_str().unwrap_or_default(), &outside)
            .is_err());
        let mut missing = RunArgs::default();
        missing
            .allow_write
            .push("/definitely/not/a/real/path".into());
        assert!(direct_run_authority(&missing).is_err());
    }

    #[test]
    fn exec_grants_follow_the_planned_write_rules() {
        let root = std::env::temp_dir();
        let exe_name = if cfg!(windows) { "cmd" } else { "sh" };
        let mut scoped = RunArgs::default();
        scoped.allow_write.push(root.to_string_lossy().into_owned());
        scoped.allow_exec.push(exe_name.into());
        let (authority, proc) = direct_run_authority(&scoped).unwrap();
        assert_eq!(authority.mode(), crate::fs::FilesystemMode::PlannedWrite);
        // a grant matches the resolved executable with any argv
        assert!(proc
            .authorize(
                exe_name,
                &["-c".into(), "echo anything".into()],
                None,
                &root.canonicalize().unwrap()
            )
            .is_ok());
        // a grant is rejected outside planned-write combinations
        assert!(parse_run_args(&[
            "--read-only".into(),
            "--allow-exec".into(),
            exe_name.into(),
            "-e".into(),
            "return 1".into()
        ])
        .is_err());
        assert!(parse_run_args(&[
            "--allow-exec".into(),
            "definitely-not-a-real-tool-xyz".into(),
            "--allow-write".into(),
            root.to_string_lossy().into_owned(),
            "-e".into(),
            "return 1".into()
        ])
        .is_ok());
        assert!(direct_run_authority(&{
            let mut args = scoped.clone();
            args.allow_exec.clear();
            args.allow_exec
                .push("definitely-not-a-real-tool-xyz".into());
            args
        })
        .is_err());
        // a grant without --allow-write still switches direct run to planned-write:
        // writes stay denied, the granted commands run — never a silent no-op
        let mut exec_only = RunArgs::default();
        exec_only.allow_exec.push(exe_name.into());
        let (authority, proc) = direct_run_authority(&exec_only).unwrap();
        assert_eq!(authority.mode(), crate::fs::FilesystemMode::PlannedWrite);
        let denied = std::path::PathBuf::from("/definitely/not/writable.txt");
        assert!(authority
            .authorize_write(denied.to_str().unwrap_or_default(), &denied)
            .is_err());
        assert!(proc
            .authorize(
                exe_name,
                &["-c".into(), "echo granted".into()],
                None,
                &root.canonicalize().unwrap()
            )
            .is_ok());
    }
}
