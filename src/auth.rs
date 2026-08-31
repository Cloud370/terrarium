//! Write preauthorization: the narrow contract between a model reply's `access` block and the
//! kernel's frozen write authority. Parsing and scope arithmetic are host facts; the user
//! decision is an explicit adapter-owned interface (`Authorizer`) — the kernel never renders a
//! prompt and only ever receives frozen authority.

use std::path::PathBuf;

use crate::fs::{
    resolve_existing, validate_user_path, FilesystemMode, RunFilesystemAuthority, WriteScope,
};

/// Request bounds from the authorization contract: at most 32 targets, 4 KiB encoded, and a
/// 200-character reason. An invalid or oversized request is a protocol error.
pub(crate) const MAX_WRITE_TARGETS: usize = 32;
pub(crate) const ACCESS_ENCODED_CAP: usize = 4 * 1024;
pub(crate) const ACCESS_REASON_CHARS: usize = 200;

/// The raw access block as it appeared in the reply, after strict JSON-shape and bound checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AccessBlock {
    pub writes: Vec<String>,
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

/// The set shown to the user for one decision: resolved targets plus the reason string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAccessRequest {
    pub targets: Vec<ResolvedTarget>,
    pub reason: String,
}

impl ResolvedAccessRequest {
    pub fn displays(&self) -> Vec<&str> {
        self.targets.iter().map(|t| t.display.as_str()).collect()
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

/// Strict JSON shape plus bounds: exactly `writes` (array of strings) and `reason` (string).
/// Filesystem-dependent checks (path syntax under resolution, symlinks, directories,
/// uniqueness) belong to `resolve_access_request`, which can also fail on replay.
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
    if object.len() != 2 || !object.contains_key("writes") || !object.contains_key("reason") {
        return Err("access block must have exactly the fields writes and reason".to_string());
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
    let mut parsed = Vec::with_capacity(writes.len());
    for entry in writes {
        let path = entry
            .as_str()
            .ok_or_else(|| "access writes must contain only strings".to_string())?;
        parsed.push(path.to_string());
    }
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
        writes: parsed,
        reason,
    })
}

/// Resolve and validate every requested target for the current mode. The rules: absolute
/// exact file paths free of `.`, `..`, and glob or directory forms; existing symbolic-link
/// targets rejected; parent aliases resolved by canonicalization; identities unique after
/// normalization. In `planned-write` a non-empty request also requires a reason.
pub(crate) fn resolve_access_request(
    block: &AccessBlock,
    mode: FilesystemMode,
) -> Result<ResolvedAccessRequest, String> {
    if block.writes.is_empty() {
        return Ok(ResolvedAccessRequest {
            targets: Vec::new(),
            reason: block.reason.clone(),
        });
    }
    if mode == FilesystemMode::PlannedWrite && block.reason.trim().is_empty() {
        return Err(
            "a non-empty write request requires a non-empty reason in planned-write mode".into(),
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
    Ok(ResolvedAccessRequest {
        targets,
        reason: block.reason.clone(),
    })
}

/// Whether a resolved target is already covered by an operator-declared scope. Covered
/// targets never reach a user prompt.
pub(crate) fn covered_by_scopes(scopes: &[WriteScope], target: &ResolvedTarget) -> bool {
    scopes.iter().any(|scope| scope.matches(&target.identity))
}

/// Freeze the run's authority: operator-declared scopes plus the approved exact paths.
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
