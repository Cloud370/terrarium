//! Write preauthorization: the narrow contract between a model reply's `access` block and the
//! kernel's frozen write authority. Parsing and scope arithmetic are host facts; the user
//! decision is an explicit adapter-owned interface (`Authorizer`) — the kernel never renders a
//! prompt and only ever receives frozen authority.

//! Write preauthorization: the narrow contract between a model reply's `access` block and
//! the kernel's frozen write authority. Parsing and scope arithmetic are host facts; the user
//! decision is an explicit adapter-owned interface (`Authorizer`) — the kernel never renders a
//! prompt and only ever receives frozen authority.
//!
//! One block covers both write-class effects: exact file targets and exact command records
//! (`{exe, argv, cwd}`). Process creation is a write-class effect — a child is not bound by
//! write scopes — so both ride the same resolve → subtract → decide → freeze lifecycle.

use std::path::{Path, PathBuf};

use crate::fs::{
    resolve_existing, validate_user_path, FilesystemMode, RunFilesystemAuthority, WriteScope,
};
use crate::proc::{self, CommandRecord, CommandSet, ProcAuthority};

/// Request bounds from the authorization contract: at most 32 write targets, at most 8
/// command records, 8 KiB encoded, and a 200-character reason. An invalid or oversized
/// request is a protocol error.
pub(crate) const MAX_WRITE_TARGETS: usize = 32;
pub(crate) const MAX_COMMANDS: usize = proc::MAX_COMMANDS;
pub(crate) const ACCESS_ENCODED_CAP: usize = 8 * 1024;
pub(crate) const ACCESS_REASON_CHARS: usize = 200;
/// One command exactly as the model declared it, after strict JSON-shape checks.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub(crate) struct DeclaredCommand {
    pub exe: String,
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The raw access block as it appeared in the reply, after strict JSON-shape and bound checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AccessBlock {
    pub writes: Vec<String>,
    pub commands: Vec<DeclaredCommand>,
    pub reason: String,
}

/// One requested write target after host resolution: the model's spelling for display, the
/// canonical identity for scope membership, and whether approval would create parents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub display: String,
    pub identity: PathBuf,
    pub parents_missing: bool,
}

/// One requested command after host resolution: the display form for prompts and journal
/// events, plus the frozen identity record the kernel matches at call time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCommand {
    pub display: String,
    pub record: CommandRecord,
}

/// The set shown to the user for one decision: resolved targets and commands plus the
/// reason string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAccessRequest {
    pub targets: Vec<ResolvedTarget>,
    pub commands: Vec<ResolvedCommand>,
    pub reason: String,
}

impl ResolvedAccessRequest {
    pub fn displays(&self) -> Vec<&str> {
        self.targets.iter().map(|t| t.display.as_str()).collect()
    }

    pub fn command_displays(&self) -> Vec<&str> {
        self.commands.iter().map(|c| c.display.as_str()).collect()
    }
}

/// The only user decisions that exist. Partial approval is not among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    Cancel,
    Unavailable,
}

/// Adapter-owned user interaction. The terminal adapter implements one decision prompt; an
/// invocation with no interactive authorizer — a pipe, CI job, or background run — implements
/// `Unavailable`. This runs before QuickJS starts; the kernel receives only frozen authority.
pub trait Authorizer: Send + Sync {
    fn decide(&self, request: &ResolvedAccessRequest) -> Decision;
}

/// Strict JSON shape plus bounds: exactly `writes` (array of strings), `reason` (string),
/// and optionally `commands` (array of `{exe, argv, cwd?}` records). Filesystem-dependent
/// checks (path syntax under resolution, symlinks, directories, uniqueness) belong to
/// `resolve_access_request`, which can also fail on replay.
pub(crate) fn parse_access_block(body: &str) -> Result<AccessBlock, String> {
    if body.len() > ACCESS_ENCODED_CAP {
        return Err(format!(
            "access request exceeds the {ACCESS_ENCODED_CAP}-byte encoded limit"
        ));
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("access block is not valid JSON: {e}"))?;
    let object = value
        .as_object()
        .ok_or_else(|| "access block must be a JSON object".to_string())?;
    let has_commands = object.contains_key("commands");
    let expected = if has_commands { 3 } else { 2 };
    if object.len() != expected || !object.contains_key("writes") || !object.contains_key("reason")
    {
        return Err(if has_commands {
            "access block must have exactly the fields writes, commands, and reason".to_string()
        } else {
            "access block must have exactly the fields writes and reason (commands is \
             optional)"
                .to_string()
        });
    }
    let writes = object["writes"]
        .as_array()
        .ok_or_else(|| "access writes must be an array of absolute file paths".to_string())?;
    if writes.len() > MAX_WRITE_TARGETS {
        return Err(format!(
            "access request lists {} write targets; the limit is {MAX_WRITE_TARGETS}",
            writes.len()
        ));
    }
    let mut parsed_writes = Vec::with_capacity(writes.len());
    for entry in writes {
        let path = entry
            .as_str()
            .ok_or_else(|| "access writes must contain only strings".to_string())?;
        parsed_writes.push(path.to_string());
    }
    let commands = if has_commands {
        let records = object["commands"].as_array().ok_or_else(|| {
            "access commands must be an array of {exe, argv, cwd?} records".to_string()
        })?;
        if records.len() > MAX_COMMANDS {
            return Err(format!(
                "access request lists {} commands; the limit is {MAX_COMMANDS}",
                records.len()
            ));
        }
        let mut parsed_commands = Vec::with_capacity(records.len());
        for record in records {
            parsed_commands.push(parse_command_record(record)?);
        }
        parsed_commands
    } else {
        Vec::new()
    };
    let reason = object["reason"]
        .as_str()
        .ok_or_else(|| "access reason must be a string".to_string())?
        .to_string();
    if reason.chars().count() > ACCESS_REASON_CHARS {
        return Err(format!(
            "access reason exceeds the {ACCESS_REASON_CHARS}-character limit"
        ));
    }
    Ok(AccessBlock {
        writes: parsed_writes,
        commands,
        reason,
    })
}

/// One `{exe, argv, cwd?}` record: exactly those keys, string exe, array-of-strings argv,
/// optional string cwd.
fn parse_command_record(record: &serde_json::Value) -> Result<DeclaredCommand, String> {
    let object = record
        .as_object()
        .ok_or_else(|| "access commands entries must be objects".to_string())?;
    if !object.contains_key("exe") || !object.contains_key("argv") {
        return Err("each command needs at least the fields exe and argv; cwd is optional".into());
    }
    if object.len() > 3 || (object.len() == 3 && !object.contains_key("cwd")) {
        return Err("command records take exactly the fields exe, argv, and cwd".into());
    }
    let exe = object["exe"]
        .as_str()
        .ok_or_else(|| "command exe must be a string".to_string())?
        .to_string();
    let argv = object["argv"]
        .as_array()
        .ok_or_else(|| "command argv must be an array of strings".to_string())?
        .iter()
        .map(|arg| {
            arg.as_str()
                .map(str::to_string)
                .ok_or_else(|| "command argv must contain only strings".to_string())
        })
        .collect::<Result<Vec<String>, String>>()?;
    let cwd = match object.get("cwd") {
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| "command cwd must be a string".to_string())?
                .to_string(),
        ),
        None => None,
    };
    Ok(DeclaredCommand { exe, argv, cwd })
}

/// Resolve and validate every requested target and command for the current mode. Write
/// targets: absolute exact file paths free of `.`, `..`, and glob or directory forms;
/// existing symbolic-link targets rejected; parent aliases resolved by canonicalization;
/// identities unique after normalization. Commands: the executable resolved through PATH
/// lookup and symlink normalization, the cwd resolved likewise (defaulting to the working
/// root), duplicates merged after resolution. In `planned-write` a non-empty request also
/// requires a reason.
pub(crate) fn resolve_access_request(
    block: &AccessBlock,
    mode: FilesystemMode,
    working_root: &Path,
) -> Result<ResolvedAccessRequest, String> {
    if block.writes.is_empty() && block.commands.is_empty() {
        return Ok(ResolvedAccessRequest {
            targets: Vec::new(),
            commands: Vec::new(),
            reason: block.reason.clone(),
        });
    }
    if mode == FilesystemMode::PlannedWrite && block.reason.trim().is_empty() {
        return Err(
            "a non-empty access request requires a non-empty reason in planned-write mode".into(),
        );
    }
    let mut targets = Vec::with_capacity(block.writes.len());
    let mut seen: Vec<PathBuf> = Vec::new();
    for raw in &block.writes {
        let path = validate_user_path(raw, true)
            .map_err(|error| format!("write target {raw}: {error}"))?;
        let parent = path
            .parent()
            .ok_or_else(|| format!("write target {raw}: a write target needs a parent directory"))?
            .to_path_buf();
        let name = path
            .file_name()
            .map(std::ffi::OsString::from)
            .ok_or_else(|| format!("write target {raw}: a write target needs a file name"))?;
        // resolve the parent by identity, then examine the final component without following
        // it: canonicalizing the full path would follow a symlinked final component and hide it
        let (base, tail) =
            resolve_existing(&parent).map_err(|error| format!("write target {raw}: {error}"))?;
        let identity = tail
            .iter()
            .fold(base.join(&name), |joined, part| joined.join(part));
        if seen
            .iter()
            .any(|other| crate::fs::same_identity(other, &identity))
        {
            return Err(format!(
                "write target {raw} duplicates an earlier target after normalization"
            ));
        }
        match std::fs::symlink_metadata(&identity) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!(
                    "write target {raw}: existing symbolic-link targets cannot be written"
                ));
            }
            Ok(meta) if meta.is_dir() => {
                return Err(format!(
                    "write target {raw}: a directory is not a write target; declare exact file paths"
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("write target {raw}: {error}")),
        }
        seen.push(identity.clone());
        targets.push(ResolvedTarget {
            display: path.to_string_lossy().replace('\\', "/"),
            identity,
            parents_missing: !tail.is_empty(),
        });
    }
    let mut commands = Vec::with_capacity(block.commands.len());
    for declared in &block.commands {
        let exe = proc::resolve_executable(&declared.exe)
            .map_err(|error| format!("command exe {:?}: {error}", declared.exe))?;
        let cwd = proc::resolve_cwd(declared.cwd.as_deref(), working_root)
            .map_err(|error| format!("command {:?}: {error}", declared.exe))?;
        let display = display_command(&exe, &declared.argv, &cwd);
        let record = CommandRecord {
            exe: exe.clone(),
            argv: declared.argv.clone(),
            cwd: cwd.clone(),
        };
        if commands
            .iter()
            .any(|existing: &ResolvedCommand| existing.record == record)
        {
            continue; // declarations deduplicate after resolution
        }
        commands.push(ResolvedCommand { display, record });
    }
    Ok(ResolvedAccessRequest {
        targets,
        commands,
        reason: block.reason.clone(),
    })
}

/// "What you read is what runs": the resolved executable, every argument in order and
/// whole, and the working directory. Nothing is truncated — the approval prompt is the
/// security boundary, and the 8 KiB access-block cap already bounds the display.
fn display_command(exe: &Path, argv: &[String], cwd: &Path) -> String {
    let quoted = |arg: &str| {
        if arg.is_empty() || arg.chars().any(|c| c.is_whitespace()) {
            format!("{arg:?}")
        } else {
            arg.to_string()
        }
    };
    let mut parts = vec![exe.to_string_lossy().replace('\\', "/")];
    parts.extend(argv.iter().map(|arg| quoted(arg)));
    format!(
        "{} (in {})",
        parts.join(" "),
        cwd.to_string_lossy().replace('\\', "/")
    )
}

/// Whether a resolved target is already covered by an operator-declared scope. Covered
/// targets never reach a user prompt.
pub(crate) fn covered_by_scopes(scopes: &[WriteScope], target: &ResolvedTarget) -> bool {
    scopes.iter().any(|scope| scope.matches(&target.identity))
}

/// Whether a resolved command is already covered by an operator `--allow-exec` pre-grant.
/// The grant matches the resolved executable only, any argv, covering both `exec` and
/// `spawn`; covered commands never reach a prompt.
pub(crate) fn covered_by_exec_grants(grants: &[PathBuf], command: &ResolvedCommand) -> bool {
    grants.contains(&command.record.exe)
}

/// Resolve one `--allow-exec NAME` operator grant at launch; a name that resolves to no
/// executable is a launch error, not a silent no-op.
pub(crate) fn operator_exec_grant(name: &str) -> Result<PathBuf, String> {
    proc::resolve_executable(name)
}

/// Freeze the run's write authority: operator-declared scopes plus the approved exact paths.
/// The result is what the kernel checks every actual write against; nothing can widen it
/// once QuickJS starts.
pub(crate) fn freeze_authority(
    operator_scopes: &[WriteScope],
    approved: &[ResolvedTarget],
) -> RunFilesystemAuthority {
    if operator_scopes.is_empty() && approved.is_empty() {
        return RunFilesystemAuthority::Scoped(Vec::new());
    }
    let mut scopes: Vec<WriteScope> = operator_scopes.to_vec();
    scopes.extend(
        approved
            .iter()
            .map(|t| WriteScope::Exact(t.identity.clone())),
    );
    RunFilesystemAuthority::Scoped(scopes)
}

/// Freeze the run's process authority: operator exec grants plus the approved command
/// records. The same shape rules as the filesystem side — nothing widens it mid-run.
pub(crate) fn freeze_proc_authority(
    operator_grants: &[PathBuf],
    approved: &[ResolvedCommand],
) -> ProcAuthority {
    ProcAuthority::Allowed(CommandSet {
        grants: operator_grants.to_vec(),
        records: approved
            .iter()
            .map(|command| command.record.clone())
            .collect(),
    })
}
