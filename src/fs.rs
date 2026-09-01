//! host.fs —— surface over the operating-system user's filesystem view: list / windowed read /
//! streaming scan / atomic write. Every path is one absolute user path. Reads use the OS user's
//! readable view. Writes are decided by the invocation's frozen `RunFilesystemAuthority`:
//! `read-only` denies every write, `planned-write` requires the resolved target to match an
//! approved exact file or an operator-declared prefix, `full-access` keeps only path validation
//! and the OS user's own permissions. The kernel executes that host-derived decision; it never
//! makes one.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::kernel::WriteSummary;
use rquickjs::function::Opt;
use rquickjs::{Ctx, Function, Object, Value};

const WRITE_SUMMARY_CAP: usize = 64;
const WRITE_SUMMARY_BYTES_CAP: usize = 8 * 1024;

#[derive(Default)]
pub(crate) struct WriteLog {
    pub(crate) items: Vec<WriteSummary>,
    pub(crate) truncated: bool,
    bytes: usize,
}

impl WriteLog {
    fn record(&mut self, summary: WriteSummary) {
        let bytes = summary.path.len() + 96;
        if self.items.len() < WRITE_SUMMARY_CAP
            && self.bytes.saturating_add(bytes) <= WRITE_SUMMARY_BYTES_CAP
        {
            self.bytes += bytes;
            self.items.push(summary);
        } else {
            self.truncated = true;
        }
    }
}

/// The one filesystem mode a trusted caller selects for an invocation. The model can observe it
/// (runtime state) but never change it: `read-only` is a hard denial, not a mode it can elevate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilesystemMode {
    ReadOnly,
    PlannedWrite,
    FullAccess,
}

impl FilesystemMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::PlannedWrite => "planned-write",
            Self::FullAccess => "full-access",
        }
    }
}

/// One frozen write scope. `Exact` is a user-approved exact file for one run; `Prefix` is an
/// operator-declared recursive directory granted at launch. Both store the canonical identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteScope {
    Exact(PathBuf),
    Prefix(PathBuf),
}

impl WriteScope {
    /// Operator `--allow-write DIR|FILE`: an existing directory becomes a recursive prefix,
    /// an existing file an exact scope. Symlinked operands resolve to their target identity —
    /// the operator argument is trusted, unlike a model-declared target.
    pub fn from_operator_spec(spec: &str) -> Result<Self, String> {
        // a DIR operand may carry the trailing separator that temp-dir APIs hand out
        // (`--allow-write /tmp/`); strip it, but keep a bare root (`/`, `C:\`) — stripping
        // that would leave a drive-relative path. Model-declared targets keep the strict
        // exact-file form.
        let stripped = spec.trim_end_matches(['/', '\\']);
        let spec = if stripped.is_empty() || stripped.ends_with(':') {
            spec
        } else {
            stripped
        };
        let path = validate_user_path(spec, true)
            .map_err(|error| format!("--allow-write {spec}: {error}"))?;
        let (base, tail) =
            resolve_existing(&path).map_err(|error| format!("--allow-write {spec}: {error}"))?;
        // joining an empty tail would leave a trailing separator and break the file case
        let identity = if tail.is_empty() {
            base
        } else {
            base.join(tail.iter().collect::<PathBuf>())
        };
        let meta = std::fs::symlink_metadata(&identity)
            .map_err(|_| format!("--allow-write {spec}: target does not exist"))?;
        if meta.is_dir() {
            Ok(Self::Prefix(identity))
        } else if meta.is_file() {
            Ok(Self::Exact(identity))
        } else {
            Err(format!(
                "--allow-write {spec}: target is neither a regular file nor a directory"
            ))
        }
    }

    pub(crate) fn matches(&self, identity: &Path) -> bool {
        match self {
            Self::Exact(path) => same_identity(path, identity),
            Self::Prefix(dir) => identity_under_prefix(identity, dir),
        }
    }

    pub fn display(&self) -> String {
        match self {
            Self::Exact(path) => path.display().to_string(),
            Self::Prefix(dir) => format!("{}/", dir.display()),
        }
    }
}

/// The authority a run's actual write calls are checked against, frozen before QuickJS starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunFilesystemAuthority {
    ReadOnly,
    Scoped(Vec<WriteScope>),
    FullAccess,
}

impl RunFilesystemAuthority {
    pub fn mode(&self) -> FilesystemMode {
        match self {
            Self::ReadOnly => FilesystemMode::ReadOnly,
            Self::Scoped(_) => FilesystemMode::PlannedWrite,
            Self::FullAccess => FilesystemMode::FullAccess,
        }
    }

    /// Deterministic write decision on the resolved target identity. Alternate spellings,
    /// symlinks, and other host functions cannot expand a frozen scope.
    pub(crate) fn authorize_write(&self, display: &str, identity: &Path) -> Result<(), String> {
        match self {
            Self::ReadOnly => Err(
                "write_denied: the invocation is read-only and every write is denied. \
                 Policy denial, not a bug — report it in your result, do not retry another way."
                    .into(),
            ),
            Self::FullAccess => Ok(()),
            Self::Scoped(scopes) if scopes.iter().any(|scope| scope.matches(identity)) => Ok(()),
            Self::Scoped(_) => Err(format!(
                "write_not_authorized: {display} matches no write scope approved for this run; \
                 declare the file in the access block or continue read-only. Alternate spellings \
                 cannot expand the frozen scope."
            )),
        }
    }
}

/// Identity comparison that folds the forms canonicalization cannot (a not-yet-existing tail on
/// a case-insensitive filesystem). Canonical identities make this a plain equality everywhere
/// except a new file's final components.
pub(crate) fn same_identity(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        let fold = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
        fold(a) == fold(b)
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

fn identity_under_prefix(identity: &Path, prefix: &Path) -> bool {
    #[cfg(windows)]
    {
        let fold = |p: &Path| p.to_string_lossy().replace('\\', "/").to_lowercase();
        let identity = fold(identity);
        let prefix = fold(prefix).trim_end_matches('/').to_string();
        identity == prefix || identity.starts_with(&format!("{prefix}/"))
    }
    #[cfg(not(windows))]
    {
        identity.starts_with(prefix)
    }
}

/// Lexical contract for every host.fs path: one absolute user path, unambiguous. Relative
/// paths, `~`, `.`/`..` segments, `//`, and — where `\` is not a separator — backslashes are
/// rejected before any filesystem access. `exact_file` additionally rejects trailing slashes
/// and glob metacharacters, because a write target is one exact file.
pub(crate) fn validate_user_path(raw: &str, exact_file: bool) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("path must not be empty".into());
    }
    if raw.starts_with('~') {
        return Err(format!("{raw}: `~` is not expanded; use the absolute path"));
    }
    if raw.contains('\0') {
        return Err(format!("{raw}: NUL is not a valid path character"));
    }
    #[cfg(windows)]
    let normalized = raw.replace('\\', "/");
    #[cfg(not(windows))]
    let normalized = raw.to_string();
    #[cfg(not(windows))]
    if normalized.contains('\\') {
        return Err(format!(
            "{raw}: backslash is not a path separator here; use a plain absolute path"
        ));
    }
    if normalized.contains("//") {
        return Err(format!(
            "{raw}: `//` is an ambiguous separator; use one slash per component"
        ));
    }
    // a single trailing slash is tolerated on read paths and normalized away; write targets
    // reject it outright below
    let trimmed = if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized.clone()
    };
    // string-level segment rules: `Path::components` silently normalizes `.` away, but the
    // contract rejects dot segments outright
    if trimmed != "/"
        && trimmed
            .split('/')
            .skip(1)
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(format!(
            "{raw}: `.`, `..`, and empty segments are not allowed; use the exact absolute path"
        ));
    }
    let path = PathBuf::from(&trimmed);
    if !path.is_absolute() {
        return Err(format!("{raw}: not an absolute path"));
    }
    if exact_file {
        if normalized.ends_with('/') && normalized.len() > 1 {
            return Err(format!(
                "{raw}: a write target must be an exact file path, not a directory"
            ));
        }
        if raw.contains(['*', '?', '[']) {
            return Err(format!(
                "{raw}: glob patterns are not write targets; declare the exact file path"
            ));
        }
        if path.components().next().is_none() || trimmed == "/" {
            return Err(format!(
                "{raw}: a write target must be a file, not the filesystem root"
            ));
        }
    }
    Ok(path)
}

/// Canonical identity for scope decisions: canonicalize the deepest existing ancestor, then
/// rejoin the missing tail. Symlinked ancestors, macOS `/tmp` aliases, and Windows drive-letter
/// and separator spellings collapse into one comparable form. Resolving through a file
/// component is an error on every platform: Windows classifies it as `NotFound`, which would
/// otherwise let the file itself canonicalize as the missing tail's anchor.
pub(crate) fn resolve_existing(path: &Path) -> Result<(PathBuf, Vec<OsString>), String> {
    let mut probe = path.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&probe) {
            Ok(canonical) => {
                if !tail.is_empty() && !canonical.is_dir() {
                    return Err(format!(
                        "cannot resolve {}: {} is not a directory",
                        path.display(),
                        canonical.display()
                    ));
                }
                return Ok((canonical, tail));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = probe.file_name().map(OsString::from) else {
                    return Err(format!("cannot resolve {}: {error}", path.display()));
                };
                let Some(parent) = probe.parent() else {
                    return Err(format!("cannot resolve {}: {error}", path.display()));
                };
                tail.insert(0, name);
                probe = parent.to_path_buf();
            }
            Err(error) => return Err(format!("cannot resolve {}: {error}", path.display())),
        }
    }
}

/// scan options — defaults follow ripgrep, the convention the model already knows:
/// gitignore respected, hidden (dot) entries skipped, binaries detected by content (NUL).
/// skip_dirs/skip_exts are EXTRA prunes the program opts into, never implicit magic.
#[derive(Debug, Clone)]
struct ScanOpts {
    skip_dirs: Vec<String>,
    skip_exts: Vec<String>,
    glob: Option<String>,
    contains: Option<String>,
    gitignore: bool,
    skip_hidden: bool,
}

impl Default for ScanOpts {
    fn default() -> Self {
        ScanOpts {
            skip_dirs: Vec::new(),
            skip_exts: Vec::new(),
            glob: None,
            contains: None,
            gitignore: true,
            skip_hidden: true,
        }
    }
}

const LINE_READ_CAP: usize = 2000; // read: per-line character cap — a minified file must not blow context
static TMP_CTR: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
struct ListEntry {
    name: String,
    entry_type: &'static str,
    size: Option<u64>,
}

impl<'js> rquickjs::IntoJs<'js> for ListEntry {
    fn into_js(self, ctx: &rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("name", self.name)?;
        obj.set("type", self.entry_type)?;
        match self.size {
            Some(size) => obj.set("size", size)?,
            None => obj.set("size", Value::new_null(ctx.clone()))?,
        }
        Ok(obj.into_value())
    }
}

fn list_dir(dir: &str) -> Result<Vec<ListEntry>, String> {
    let p = validate_user_path(dir, false)?;
    let mut out = Vec::new();
    for ent in std::fs::read_dir(&p).map_err(|e| format!("{dir}: {e}"))? {
        let ent = ent.map_err(|e| format!("{dir}: {e}"))?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let meta = std::fs::symlink_metadata(ent.path()).map_err(|e| format!("{dir}: {e}"))?;
        let file_type = meta.file_type();
        let (entry_type, size) = if file_type.is_symlink() {
            ("symlink", None)
        } else if meta.is_dir() {
            ("directory", None)
        } else if meta.is_file() {
            ("file", Some(meta.len()))
        } else {
            ("other", None)
        };
        out.push(ListEntry {
            name,
            entry_type,
            size,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn read_line_limited(
    reader: &mut BufReader<std::fs::File>,
    cap: usize,
) -> std::io::Result<Option<String>> {
    let mut bytes = Vec::new();
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                String::from_utf8(bytes)
                    .map(Some)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
            };
        }
        let take = buf
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|index| index + 1)
            .unwrap_or(buf.len());
        if bytes.len() + take > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("line exceeds the {cap}-byte read limit"),
            ));
        }
        let has_newline = buf[..take].contains(&b'\n');
        bytes.extend_from_slice(&buf[..take]);
        reader.consume(take);
        if has_newline {
            return String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e));
        }
    }
}

/// Windowed read: "N: text" lines from..to (1-based, inclusive), a continue-footer iff more lines follow.
fn read_window(path: &str, a: usize, b: usize) -> Result<String, String> {
    if a == 0 || b < a {
        return Err(format!(
            "read window must satisfy 1 <= from <= to (got {a}..{b})"
        ));
    }
    let p = validate_user_path(path, false)?;
    let meta = std::fs::metadata(&p).map_err(|e| format!("{path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path} is not a regular file"));
    }
    let mut budget: isize = crate::MEM_LIMIT as isize;
    let f = std::fs::File::open(&p).map_err(|e| format!("{path}: {e}"))?;
    let mut reader = BufReader::new(f);
    let mut out = Vec::new();
    let mut more = false;
    let mut line_no = 0usize;
    while let Some(raw) =
        read_line_limited(&mut reader, 8 * 1024 * 1024).map_err(|e| format!("{path}: {e}"))?
    {
        line_no += 1;
        if line_no > b {
            more = true;
            break;
        }
        if line_no >= a {
            let line = raw
                .strip_suffix('\n')
                .unwrap_or(&raw)
                .strip_suffix('\r')
                .unwrap_or_else(|| raw.strip_suffix('\n').unwrap_or(&raw));
            budget -= line.len() as isize + 8;
            if budget < 0 {
                return Err(format!(
                    "read window on {path} exceeded the 64MB budget — narrow the range"
                ));
            }
            if line.chars().count() > LINE_READ_CAP {
                out.push(format!(
                    "{line_no}: {}…",
                    line.chars().take(LINE_READ_CAP).collect::<String>()
                ));
            } else {
                out.push(format!("{line_no}: {line}"));
            }
        }
    }
    let mut s = out.join("\n");
    if more {
        s.push_str(&format!(
            "\n[more lines follow — continue with host.fs.read(\"{path}\", {}, …)]",
            b + 1
        ));
    }
    Ok(s)
}

/// Whole-file text channel for PROGRAM consumption: LF-normalized, BOM-stripped (the in-program
/// canonical form — write restores the target's own EOL and BOM), no line numbers or
/// line caps. Bounded by the cage heap: a file bigger than 64MB is refused, not buffered.
fn read_text(path: &str) -> Result<String, String> {
    let p = validate_user_path(path, false)?;
    let meta = std::fs::metadata(&p).map_err(|e| format!("{path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path} is not a regular file"));
    }
    if meta.len() > crate::MEM_LIMIT as u64 {
        return Err(format!(
            "{path} is {} bytes — larger than the 64MB cage heap; use windowed host.fs.read \
             or host.fs.scan",
            meta.len()
        ));
    }
    std::fs::read_to_string(p)
        .map(|s| {
            s.strip_prefix('\u{feff}')
                .unwrap_or(&s)
                .replace("\r\n", "\n")
        })
        .map_err(|e| format!("{path}: {e}"))
}

// ===== scan: chunked async line stream — the predicate lives in the cage =====
// The host side is deliberately dumb: walk a scoped tree, filter by skip-lists + glob, hand out ~1000-line
// chunks per await. Matching/casing/dedup/max are JS combinators, so the API surface stays flat while the
// expressiveness is "whatever JS can write". Chunked handoff is also what keeps the deadline honest: the
// runtime yields between chunks, so a huge tree streams instead of freezing the process inside one poll.

const SCAN_CHUNK: usize = 1000; // lines per await — small enough that no single chunk blocks the loop for long
const SCAN_CHUNK_BYTES: usize = 1024 * 1024; // chunk completes at 1000 lines OR this many bytes, whichever first
                                             // Per-line read bound ≈ what the 64MB cage heap could meaningfully hold anyway; a no-newline giant
                                             // (2GB base64 blob) stops here instead of allocating unboundedly. Lines are delivered WHOLE —
                                             // match completeness belongs to the predicate (grep matches the full line, truncates only the display),
                                             // context economy is already guarded downstream by the feedback cap. Heap is the only bound.
const SCAN_LINE_HARD: u64 = 8 * 1024 * 1024;

/// Tiny glob: `*` = any run within one segment, `**` = anything incl. `/`, `?` = one non-`/` char.
/// A pattern containing `/` matches the path relative to the scan root; otherwise the basename
/// (so "*.rs" behaves like grep --include, "src/**/*.rs" like rg --glob).
/// Iterative DP over three rolling rows — O(P×T), same answers as the classic backtracking
/// recursion but with no exponential cliff (`*a*a*a*b` against long names froze a whole poll:
/// the interrupt handler only fires at JS bytecode boundaries, never inside native code) and
/// no native-stack recursion (a multi-MB pattern once segfaulted the process before any limit).
fn glob_match(pat: &[char], text: &[char]) -> bool {
    let nt = text.len();
    // rows[i % 3][j] = "pat[i..] matches text[j..]"; row i is built from rows i+1 and i+2.
    let mut rows = [
        vec![false; nt + 1],
        vec![false; nt + 1],
        vec![false; nt + 1],
    ];
    rows[pat.len() % 3][nt] = true; // empty pattern matches only empty text
    for i in (0..pat.len()).rev() {
        let cur = i % 3;
        for j in (0..=nt).rev() {
            rows[cur][j] = match pat[i] {
                // `**/` may match zero directory segments.
                '*' if pat.get(i + 1) == Some(&'*') && pat.get(i + 2) == Some(&'/') => {
                    rows[(i + 3) % 3][j] || (j < nt && rows[cur][j + 1])
                }
                // `**` matches any run, including `/`.
                '*' if pat.get(i + 1) == Some(&'*') => {
                    rows[(i + 2) % 3][j] || (j < nt && rows[cur][j + 1])
                }
                // `*`: match nothing, or consume one non-'/' char and stay put
                '*' => rows[(i + 1) % 3][j] || (j < nt && text[j] != '/' && rows[cur][j + 1]),
                '?' => j < nt && text[j] != '/' && rows[(i + 1) % 3][j + 1],
                c => j < nt && text[j] == c && rows[(i + 1) % 3][j + 1],
            };
        }
    }
    rows[0][0]
}

/// Patterns come from two untrusted sources (program args, mounted .gitignore files); the DP is
/// linear in pattern length, so cap it where patterns enter. No real-world glob or gitignore
/// line is anywhere near this long.
const GLOB_PAT_MAX: usize = 1024;

fn file_matches_glob(pat: &str, rel: &str) -> bool {
    if pat.chars().count() > GLOB_PAT_MAX || rel.chars().count() > GLOB_PAT_MAX * 8 {
        return false;
    }
    let (p, t): (Vec<char>, Vec<char>) = if pat.contains('/') {
        (pat.chars().collect(), rel.chars().collect())
    } else {
        (
            pat.chars().collect(),
            rel.rsplit('/').next().unwrap_or(rel).chars().collect(),
        )
    };
    glob_match(&p, &t)
}

// ---- gitignore respect (rg semantics, as a parameter the model decides per call) ----
// Scope pruning like glob, not judgment: ignored trees are simply never opened. Hand-rolled
// on the common subset — anchoring, `**`, basename-anywhere, dir-only, negation, nested scopes.
// Punted: `[..]` char classes and `\!` escapes (rare); .gitignore applies only at/below the scan
// root (no upward repo search — walk_one_dir never sees parents).

#[derive(Debug, Clone)]
struct IgnorePat {
    neg: bool,
    dir_only: bool,
    anchored: bool, // contains '/' (besides a trailing one) → match the rel path from the .gitignore's dir
    pat: Vec<char>,
}

/// One .gitignore file's patterns, anchored at the directory holding it (rel to the scan root).
#[derive(Debug, Clone)]
struct IgnoreScope {
    base: String, // "" = scan root
    pats: Vec<IgnorePat>,
}

fn parse_gitignore(text: &str) -> Vec<IgnorePat> {
    text.lines()
        .filter_map(|raw| {
            let l = raw.trim_end(); // git drops unescaped trailing spaces; close enough
            if l.is_empty() || l.starts_with('#') || l.chars().count() > GLOB_PAT_MAX {
                return None; // over-long lines are hostile input, not ignore rules (glob DP is linear in P)
            }
            let (neg, l) = match l.strip_prefix('!') {
                Some(r) => (true, r),
                None => (false, l),
            };
            let (dir_only, l) = match l.strip_suffix('/') {
                Some(r) => (true, r),
                None => (false, l),
            };
            if l.is_empty() {
                return None;
            }
            let anchored = l.contains('/');
            let l = l.strip_prefix('/').unwrap_or(l);
            Some(IgnorePat {
                neg,
                dir_only,
                anchored,
                pat: l.chars().collect(),
            })
        })
        .collect()
}

/// gitignore's precedence — deepest scope wins, and within a file the LAST matching line decides
/// (so `!un_ignore` after an ignore re-includes): walk both reversed, first decisive hit answers.
fn entry_ignored(scopes: &[IgnoreScope], rel_from_root: &str, name: &str, is_dir: bool) -> bool {
    let name_c: Vec<char> = name.chars().collect();
    for sc in scopes.iter().rev() {
        let rel = if sc.base.is_empty() {
            rel_from_root
        } else {
            match rel_from_root.strip_prefix(&format!("{}/", sc.base)) {
                Some(r) => r,
                None => continue, // entry isn't under this scope's dir
            }
        };
        let rel_c: Vec<char> = rel.chars().collect();
        for p in sc.pats.iter().rev() {
            if p.dir_only && !is_dir {
                continue;
            }
            let m = if p.anchored {
                glob_match(&p.pat, &rel_c)
            } else {
                glob_match(&p.pat, &name_c)
            };
            if m {
                return !p.neg;
            }
        }
    }
    false
}

/// Load dir/.gitignore as a scope (base = dir rel to scan root); empty when absent.
/// A symlinked rule file is not read — following it could pull ignore rules from outside the mount.
fn load_scope(dir: &Path, root: &Path, label: &str) -> Result<IgnoreScope, String> {
    let base = dir
        .strip_prefix(root)
        .unwrap_or(dir)
        .to_string_lossy()
        .replace('\\', "/");
    let gi = dir.join(".gitignore");
    let text = match std::fs::symlink_metadata(&gi) {
        Ok(md) if md.file_type().is_symlink() => String::new(),
        Ok(_) => std::fs::read_to_string(&gi)
            .map_err(|e| format!("{label} {base}/.gitignore: cannot read ignore file: {e}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(format!(
                "{label} {base}/.gitignore: cannot inspect ignore file: {e}"
            ))
        }
    };
    Ok(IgnoreScope {
        base,
        pats: parse_gitignore(&text),
    })
}

#[derive(Debug)]
struct ScanLine {
    file: String,
    no: usize,
    text: String,
}

impl<'js> rquickjs::IntoJs<'js> for ScanLine {
    fn into_js(self, ctx: &rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("file", self.file)?;
        obj.set("no", self.no as u32)?;
        obj.set("text", self.text)?;
        Ok(obj.into_value())
    }
}

/// One candidate file from the walker: real path to open, configured path to report, size in bytes.
/// walk yields these as-is; scan opens `.real` and streams `.virt`'s lines.
#[derive(Debug)]
struct ScanFile {
    real: PathBuf,
    virt: String,
    size: u64,
}

/// Walk state for one scan or walk: directories still to enumerate, candidate files queued,
/// current reader (scan only). Nothing is cached across calls — state lives exactly as long
/// as the iterator the program holds.
#[derive(Debug)]
struct ScanState {
    label: &'static str, // "scan" | "walk" — error provenance for the model
    dir_queue: Vec<(PathBuf, Vec<IgnoreScope>)>,
    root: PathBuf,
    virt_base: String,
    files: VecDeque<ScanFile>,
    cur: Option<(BufReader<std::fs::File>, String)>,
    lineno: usize,
    opts: ScanOpts,
    done: bool,
}

/// scan and walk share one traversal engine; only the yield unit differs (lines vs entries).
fn open_tree(js_path: &str, opts: ScanOpts, label: &'static str) -> Result<ScanState, String> {
    let display_base = validate_user_path(js_path, false)?;
    let real = display_base
        .canonicalize()
        .map_err(|e| format!("{js_path}: {e}"))?;
    let meta = std::fs::metadata(&real).map_err(|e| format!("{js_path}: {e}"))?;
    if !meta.is_dir() {
        return Err(format!(
            "{js_path} is not a directory — walk and scan traverse a tree (single files go through host.fs.read)"
        ));
    }
    Ok(ScanState {
        label,
        dir_queue: vec![(real.clone(), Vec::new())],
        root: real,
        virt_base: display_base.to_string_lossy().replace('\\', "/"),
        files: VecDeque::new(),
        cur: None,
        lineno: 0,
        opts,
        done: false,
    })
}

fn scan_open(js_path: &str, opts: ScanOpts) -> Result<ScanState, String> {
    open_tree(js_path, opts, "scan")
}

fn walk_open(js_path: &str, opts: ScanOpts) -> Result<ScanState, String> {
    open_tree(js_path, opts, "walk")
}

fn scan_virtual_path(st: &ScanState, path: &Path) -> String {
    let rel = path
        .strip_prefix(&st.root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    if rel.is_empty() {
        st.virt_base.clone()
    } else {
        format!("{}/{}", st.virt_base, rel)
    }
}

/// Enumerate exactly one directory per call — bounded work per chunk, so the walk can't wedge a poll either.
fn walk_one_dir(st: &mut ScanState) -> Result<(), String> {
    let Some((dir, scopes)) = st.dir_queue.pop() else {
        st.done = true;
        return Ok(());
    };
    let label = st.label;
    let display = scan_virtual_path(st, &dir);
    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("{label} {display}: cannot read directory: {e}"))?;
    // this dir's own .gitignore joins the inherited scopes for its entries and all descendants
    let mut child_scopes = scopes;
    if st.opts.gitignore {
        let sc = load_scope(&dir, &st.root, label)?;
        if !sc.pats.is_empty() {
            child_scopes.push(sc);
        }
    }
    let mut subdirs = Vec::new();
    let mut found = Vec::new();
    for ent in entries {
        let ent =
            ent.map_err(|e| format!("{label} {display}: cannot inspect directory entry: {e}"))?;
        let name = ent.file_name().to_string_lossy().into_owned();
        // hidden first (cheapest, rg's default), then explicit extras, then gitignore
        if st.opts.skip_hidden && name.starts_with('.') {
            continue;
        }
        // classify by the directory entry itself, never by following it: a symlinked dir/file
        // would walk or read outside the mount (rg's own default — no --follow), and a FIFO
        // would block the open with no deadline to rescue it. Only real dirs and regular files
        // are scan material.
        let ft = ent.file_type().map_err(|e| {
            format!(
                "{} {}: cannot inspect entry: {e}",
                label,
                scan_virtual_path(st, &ent.path())
            )
        })?;
        if ft.is_symlink() {
            continue;
        }
        let p = ent.path();
        let rel = p
            .strip_prefix(&st.root)
            .unwrap_or(&p)
            .to_string_lossy()
            .replace('\\', "/");
        if ft.is_dir() {
            if st
                .opts
                .skip_dirs
                .iter()
                .any(|d| d.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            if st.opts.gitignore && entry_ignored(&child_scopes, &rel, &name, true) {
                continue;
            }
            subdirs.push((p, child_scopes.clone()));
        } else if ft.is_file() {
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if st.opts.skip_exts.contains(&ext) {
                continue;
            }
            if st.opts.gitignore && entry_ignored(&child_scopes, &rel, &name, false) {
                continue;
            }
            if let Some(g) = &st.opts.glob {
                if !file_matches_glob(g, &rel) {
                    continue;
                }
            }
            // size from the directory entry itself — walk never opens the file
            let size = ent
                .metadata()
                .map_err(|e| {
                    format!(
                        "{label} {}: cannot inspect file: {e}",
                        scan_virtual_path(st, &p)
                    )
                })?
                .len();
            found.push(ScanFile {
                real: p,
                virt: format!("{}/{}", st.virt_base, rel),
                size,
            });
        }
    }
    subdirs.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic walk order: by path, scopes just ride along
    found.sort_by(|a, b| a.virt.cmp(&b.virt));
    st.dir_queue.extend(subdirs);
    st.files.extend(found);
    Ok(())
}

#[derive(Debug)]
struct ScanBatch {
    items: Vec<ScanLine>,
    done: bool,
}

impl<'js> rquickjs::IntoJs<'js> for ScanBatch {
    fn into_js(self, ctx: &rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("items", self.items)?;
        obj.set("done", self.done)?;
        Ok(obj.into_value())
    }
}

fn scan_is_done(st: &ScanState) -> bool {
    st.done && st.files.is_empty() && st.cur.is_none()
}

fn scan_next_batch(st: &mut ScanState) -> Result<ScanBatch, String> {
    let items = scan_next_chunk(st)?;
    Ok(ScanBatch {
        items,
        done: scan_is_done(st),
    })
}

/// Next bounded batch of scan input. With a literal prefilter, an empty batch may be non-final;
/// callers must use `ScanBatch.done` rather than treating an empty `items` array as EOF.
fn scan_next_chunk(st: &mut ScanState) -> Result<Vec<ScanLine>, String> {
    let mut lines = Vec::new();
    let mut bytes = 0usize;
    let mut scanned_bytes = 0usize;
    while lines.len() < SCAN_CHUNK && bytes < SCAN_CHUNK_BYTES && scanned_bytes < SCAN_CHUNK_BYTES {
        if st.cur.is_none() {
            while st.files.is_empty() && !st.done {
                walk_one_dir(st)?;
            }
            if st.files.is_empty() {
                break;
            }
            let (p, v) = match st.files.pop_front() {
                Some(ScanFile { real, virt, .. }) => (real, virt),
                None => break,
            };
            let f =
                std::fs::File::open(&p).map_err(|e| format!("scan {v}: cannot open file: {e}"))?;
            let mut r = BufReader::new(f);
            // binary = NUL in the first buffer (grep/rg's physical definition, not judgment);
            // the buffer isn't consumed — the reads below reuse it, so the sniff costs nothing.
            // scan is a text channel: binary bytes were never expressible as lines, only wastefully
            // opened — real byte inspection needs its own windowed channel (deferred)
            let is_binary = r
                .fill_buf()
                .map_err(|e| format!("scan {v}: cannot inspect file: {e}"))?
                .contains(&0);
            if is_binary {
                continue;
            }
            st.lineno = 0;
            st.cur = Some((r, v));
        }
        let contains = st.opts.contains.clone();
        let mut rotate = false;
        if let Some((r, vpath)) = st.cur.as_mut() {
            let vpath = vpath.clone();
            for _ in 0..(SCAN_CHUNK - lines.len()) {
                let mut raw = String::new();
                // bounded read: a pathological no-newline line stops at the hard cap instead of
                // allocating the whole file (a 2GB embedded base64 blob is read as 8MB + drained)
                let read = r.by_ref().take(SCAN_LINE_HARD + 1).read_line(&mut raw);
                match read {
                    Ok(0) => {
                        rotate = true;
                        break;
                    }
                    Ok(_) => {
                        st.lineno += 1;
                        // "hit the cap" = limit bytes with no newline: the physical line continues past
                        // the bound (a line of exactly SCAN_LINE_HARD+1 bytes ending in \n is complete)
                        if !raw.ends_with('\n') && raw.len() as u64 > SCAN_LINE_HARD {
                            // deliver the capped prefix, marked as cut — the remainder is drained in
                            // buffer-sized steps until the line's end or EOF (it can't be matched)
                            raw.push('…');
                            while let Ok(buf) = r.fill_buf() {
                                if buf.is_empty() {
                                    break;
                                }
                                match buf.iter().position(|&c| c == b'\n') {
                                    Some(i) => {
                                        r.consume(i + 1);
                                        break;
                                    }
                                    None => {
                                        let l = buf.len();
                                        r.consume(l);
                                    }
                                }
                            }
                        }
                        // deliver the line whole — a minified one-liner arrives as one long line and
                        // the predicate sees all of it (grep matches the full line, truncates only
                        // the display); context economy is guarded downstream by the feedback cap
                        if raw.ends_with('\n') {
                            raw.pop();
                            if raw.ends_with('\r') {
                                raw.pop();
                            }
                        }
                        scanned_bytes += raw.len();
                        if let Some(contains) = &contains {
                            if !raw.contains(contains) {
                                if scanned_bytes >= SCAN_CHUNK_BYTES {
                                    break;
                                }
                                continue;
                            }
                        }
                        bytes += raw.len();
                        lines.push(ScanLine {
                            file: vpath.clone(),
                            no: st.lineno,
                            text: raw,
                        });
                        // the byte budget must bind where the allocations happen: this inner loop
                        // could otherwise stack 1000 × 8MB monster lines before the outer check
                        if bytes >= SCAN_CHUNK_BYTES || scanned_bytes >= SCAN_CHUNK_BYTES {
                            break;
                        }
                    }
                    Err(e) => {
                        return Err(format!("scan {vpath}: invalid UTF-8 or read failure: {e}"));
                    }
                }
            }
        }
        if rotate {
            st.cur = None;
        }
    }
    Ok(lines)
}

/// One yielded file entry for walk: the configured path and its size in bytes.
/// Deliberately not list-shaped — every entry IS a regular file, so a constant
/// `type` field would be noise; counting yields counts files, by construction.
#[derive(Debug)]
struct WalkEntry {
    file: String,
    size: u64,
}

impl<'js> rquickjs::IntoJs<'js> for WalkEntry {
    fn into_js(self, ctx: &rquickjs::Ctx<'js>) -> rquickjs::Result<rquickjs::Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("file", self.file)?;
        obj.set("size", self.size)?;
        Ok(obj.into_value())
    }
}

/// Next chunk of file entries for walk; an EMPTY chunk is the exhaustion signal.
/// Same walker as scan, one layer up: pruning already happened in walk_one_dir,
/// and files are never opened here — no content, no binary sniffing, no UTF-8 risk.
fn walk_next_chunk(st: &mut ScanState) -> Result<Vec<WalkEntry>, String> {
    let mut entries = Vec::new();
    while entries.len() < SCAN_CHUNK {
        while st.files.is_empty() && !st.done {
            walk_one_dir(st)?;
        }
        if st.files.is_empty() {
            break;
        }
        match st.files.pop_front() {
            Some(ScanFile { virt, size, .. }) => entries.push(WalkEntry { file: virt, size }),
            None => break,
        }
    }
    Ok(entries)
}

/// Existing target's shape, from a 4KB sample (dsh detectLineEndings semantics): leading BOM + majority EOL.
/// The in-program canonical form is LF (read strips \r\n), so write restores what the file had.
fn detect_shape(target: &Path) -> (bool, bool) {
    let Ok(mut f) = std::fs::File::open(target) else {
        return (false, false);
    };
    let mut sample = vec![0u8; 4096];
    let n = f.read(&mut sample).unwrap_or(0);
    let sample = &sample[..n];
    let bom = sample.starts_with(&[0xEF, 0xBB, 0xBF]);
    let crlf = sample.windows(2).filter(|w| w == b"\r\n").count();
    let lf = sample.iter().filter(|&&b| b == b'\n').count() - crlf;
    (bom, crlf > lf)
}

#[cfg(test)]
fn write_file(
    authority: &RunFilesystemAuthority,
    js_path: &str,
    content: &str,
) -> Result<usize, String> {
    write_file_with_log(authority, js_path, content, &mut WriteLog::default())
}

struct BeforeFileSummary {
    bytes: u64,
    changed: bool,
    first_changed_line: Option<usize>,
}

fn inspect_before(target: &Path, after: &[u8]) -> Result<Option<BeforeFileSummary>, String> {
    let metadata = match std::fs::metadata(target) {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let mut file = std::fs::File::open(target).map_err(|error| error.to_string())?;
    let mut buffer = [0_u8; 64 * 1024];
    let mut offset = 0usize;
    let mut line = 1usize;
    let mut first_changed_line = None;
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        for &byte in &buffer[..read] {
            if first_changed_line.is_none() && after.get(offset) != Some(&byte) {
                first_changed_line = Some(line);
            }
            if byte == b'\n' {
                line += 1;
            }
            offset += 1;
        }
    }
    if first_changed_line.is_none() && offset != after.len() {
        first_changed_line = Some(line);
    }
    Ok(Some(BeforeFileSummary {
        bytes: metadata.len(),
        changed: first_changed_line.is_some(),
        first_changed_line,
    }))
}

/// Atomic write under the frozen authority: lexical validation → mode → symlink/directory
/// target checks → canonical identity + scope membership → create approved missing parents →
/// recheck identity and target state under the final parent → temp+rename. Preserves an
/// existing target's BOM and line-ending style; returns bytes written and records a bounded
/// receipt. Approving a new-file target subsumes creating its missing parent directories.
fn write_file_with_log(
    authority: &RunFilesystemAuthority,
    js_path: &str,
    content: &str,
    writes: &mut WriteLog,
) -> Result<usize, String> {
    let path = validate_user_path(js_path, true)?;
    if let RunFilesystemAuthority::ReadOnly = authority {
        return Err(
            "write_denied: the invocation is read-only and every write is denied. \
             Policy denial, not a bug — report it in your result, do not retry another way."
                .into(),
        );
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("{js_path}: a write target needs a parent directory"))?
        .to_path_buf();
    let name = path
        .file_name()
        .map(OsString::from)
        .ok_or_else(|| format!("{js_path}: a write target needs a file name"))?;
    let (canon_base, missing) = resolve_existing(&parent)?;
    let identity = missing
        .iter()
        .fold(canon_base.join(&name), |joined, part| joined.join(part));
    // target state under the resolved parent: existing symbolic links and directories are
    // never written; a missing target is the new-file case the approval covered.
    let target_meta = std::fs::symlink_metadata(&identity);
    match &target_meta {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(format!(
                "write_denied: {js_path} is a symbolic link; existing symbolic-link targets cannot be written"
            ));
        }
        Ok(meta) if meta.is_dir() => {
            return Err(format!(
                "{js_path} is a directory; write an exact file path"
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{js_path}: {error}")),
    }
    authority.authorize_write(js_path, &identity)?;
    std::fs::create_dir_all(&parent).map_err(|e| format!("{js_path}: {e}"))?;
    let canon_parent = parent
        .canonicalize()
        .map_err(|e| format!("{js_path}: {e}"))?;
    let target = canon_parent.join(&name);
    // recheck: creating missing parents (or a concurrent filesystem change) may have shifted
    // the identity or swapped the target state after the first decision.
    if !same_identity(&target, &identity) {
        authority.authorize_write(js_path, &target)?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(format!(
                "write_denied: {js_path} became a symbolic link before the write; rejected"
            ));
        }
    }
    let target_meta = std::fs::symlink_metadata(&target);
    let created = target_meta
        .as_ref()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
    // symlink targets were rejected above; new/regular targets carry their own shape
    let (bom, crlf) = match &target_meta {
        Ok(md) if md.file_type().is_symlink() => (false, false),
        _ => detect_shape(&target),
    };
    let mut body = content.replace("\r\n", "\n"); // canonicalize; lone \r is left untouched
    if crlf {
        body = body.replace('\n', "\r\n");
    }
    let mut out = String::new();
    if bom && !body.starts_with('\u{feff}') {
        out.push('\u{feff}');
    }
    out.push_str(&body);
    let before = match &target_meta {
        Ok(md) if md.file_type().is_symlink() => None,
        Ok(_) => match inspect_before(&target, out.as_bytes()) {
            Ok(summary) => summary,
            Err(_) => Some(BeforeFileSummary {
                bytes: std::fs::metadata(&target)
                    .map(|metadata| metadata.len())
                    .unwrap_or(0),
                changed: true,
                first_changed_line: Some(1),
            }),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(format!("{js_path}: {error}")),
    };
    let tmp = canon_parent.join(format!(
        ".terrarium-{}-{}",
        std::process::id(),
        TMP_CTR.fetch_add(1, Ordering::Relaxed)
    ));
    let mut tmp_file = std::fs::OpenOptions::new();
    tmp_file.write(true).create_new(true);
    let mut tmp_file = tmp_file.open(&tmp).map_err(|e| format!("{js_path}: {e}"))?;
    tmp_file
        .write_all(out.as_bytes())
        .map_err(|e| format!("{js_path}: {e}"))?;
    tmp_file.sync_all().map_err(|e| format!("{js_path}: {e}"))?;
    drop(tmp_file);
    // brief backoff retries: transient holders (AV scanners, indexers, editors — the classic Windows
    // sharing violation, same mitigation git/cargo use); persistent holders still surface as an error
    let mut err = None;
    for attempt in 0..3 {
        match std::fs::rename(&tmp, &target) {
            Ok(()) => {
                err = None;
                break;
            }
            Err(e) => {
                err = Some(e);
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
    if let Some(e) = err {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{js_path}: {e} (after 3 attempts — a lingering editor/antivirus handle is the usual culprit)"));
    }
    writes.record(WriteSummary {
        path: js_path.to_string(),
        created,
        changed: before.as_ref().is_none_or(|before| before.changed),
        bytes_before: before.as_ref().map(|before| before.bytes),
        bytes_after: out.len() as u64,
        first_changed_line: before
            .as_ref()
            .and_then(|before| before.first_changed_line)
            .or_else(|| before.is_none().then_some(1)),
    });
    Ok(out.len())
}

fn js_err(msg: String) -> rquickjs::Error {
    rquickjs::Error::FromJs {
        from: "fs",
        to: "result",
        message: Some(msg),
    }
}

/// Shared option parsing for scan and walk — the options are traversal-level, so the two
/// APIs accept the exact same set (defaults follow ripgrep).
fn parse_tree_opts(o: &Object<'_>, allow_contains: bool) -> rquickjs::Result<ScanOpts> {
    let mut so = ScanOpts::default(); // ripgrep defaults: gitignore respected, dot-entries skipped
    if let Some(value) = o
        .get::<_, Option<String>>("contains")
        .map_err(|e| js_err(format!("contains must be a string: {e}")))?
    {
        if value.is_empty() {
            return Err(js_err("contains must not be empty".to_string()));
        }
        if !allow_contains {
            return Err(js_err(
                "contains is only supported by host.fs.scan; host.fs.walk never opens files"
                    .to_string(),
            ));
        }
        so.contains = Some(value);
    }
    so.skip_dirs = o
        .get::<_, Option<Vec<String>>>("skipDirs")
        .map_err(|e| js_err(format!("skipDirs must be an array of strings: {e}")))?
        .unwrap_or_default();
    so.skip_exts = o
        .get::<_, Option<Vec<String>>>("skipExts")
        .map_err(|e| js_err(format!("skipExts must be an array of strings: {e}")))?
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.to_lowercase())
        .collect();
    if let Some(v) = o
        .get::<_, Option<String>>("glob")
        .map_err(|e| js_err(format!("glob must be a string: {e}")))?
    {
        if v.chars().count() > GLOB_PAT_MAX {
            return Err(js_err(format!(
                "glob pattern longer than {GLOB_PAT_MAX} chars — not a real-world glob"
            )));
        }
        so.glob = Some(v);
    }
    if let Some(v) = o
        .get::<_, Option<bool>>("gitignore")
        .map_err(|e| js_err(format!("gitignore must be a boolean: {e}")))?
    {
        so.gitignore = v;
    }
    if let Some(v) = o
        .get::<_, Option<bool>>("hidden")
        .map_err(|e| js_err(format!("hidden must be a boolean: {e}")))?
    {
        so.skip_hidden = !v; // {hidden: true} includes dot-entries, rg's --hidden polarity
    }
    Ok(so)
}

/// Registers the host.fs namespace. Read capabilities are unconditional (the OS user's
/// readable view); the write capability enforces the frozen run authority.
pub fn install<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    authority: &RunFilesystemAuthority,
    writes: Rc<RefCell<WriteLog>>,
) -> rquickjs::Result<()> {
    let fsobj = Object::new(ctx.clone())?;

    let list_fn = Function::new(ctx.clone(), move |dir: String| {
        list_dir(&dir).map_err(js_err)
    })?;
    fsobj.set("list", list_fn)?;

    // window-only: an explicit line range per read — to=Infinity reads to EOF
    let read_fn = Function::new(
        ctx.clone(),
        move |path: String, from: Opt<f64>, to: Opt<f64>| {
            let (a, b) = match (from.0, to.0) {
            (Some(a), Some(b)) => (a.max(1.0) as usize, b.max(0.0) as usize),
            _ => {
                return Err(js_err(format!(
                    "read(path, from, to) needs an explicit line window, e.g. host.fs.read(\"{path}\", 1, 300); to=Infinity reads to EOF"
                )))
            }
        };
            read_window(&path, a, b).map_err(js_err)
        },
    )?;
    fsobj.set("read", read_fn)?;

    // scan and walk share one traversal engine: scan streams the tree's LINES,
    // walk streams its FILE ENTRIES. next() resolves to an array; empty = exhausted.
    // The async-iterator blessing (Symbol.asyncIterator, chunk flattening) lives in prelude.js.
    let scan_fn = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, path: String, opts: Opt<Object>| -> rquickjs::Result<Object> {
            let so = match opts.0 {
                Some(o) => parse_tree_opts(&o, true)?,
                None => ScanOpts::default(),
            };
            let st = scan_open(&path, so).map_err(js_err)?;
            let rc: Rc<RefCell<ScanState>> = Rc::new(RefCell::new(st));
            let next_fn = Function::new(
                ctx.clone(),
                rquickjs::function::Async(move || {
                    let rc = rc.clone(); // clone per call: the closure must stay Fn
                    async move { scan_next_batch(&mut rc.borrow_mut()).map_err(js_err) }
                }),
            )?;
            let it = Object::new(ctx.clone())?;
            it.set("next", next_fn)?;
            Ok(it)
        },
    )?;
    fsobj.set("scan", scan_fn)?;

    let walk_fn = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, path: String, opts: Opt<Object>| -> rquickjs::Result<Object> {
            let so = match opts.0 {
                Some(o) => parse_tree_opts(&o, false)?,
                None => ScanOpts::default(),
            };
            let st = walk_open(&path, so).map_err(js_err)?;
            let rc: Rc<RefCell<ScanState>> = Rc::new(RefCell::new(st));
            let next_fn = Function::new(
                ctx.clone(),
                rquickjs::function::Async(move || {
                    let rc = rc.clone();
                    async move { walk_next_chunk(&mut rc.borrow_mut()).map_err(js_err) }
                }),
            )?;
            let it = Object::new(ctx.clone())?;
            it.set("next", next_fn)?;
            Ok(it)
        },
    )?;
    fsobj.set("walk", walk_fn)?;

    let text_fn = Function::new(ctx.clone(), move |path: String| {
        read_text(&path).map_err(js_err)
    })?;
    fsobj.set("text", text_fn)?;

    let authority = authority.clone();
    let write_log = writes.clone();
    let write_fn = Function::new(ctx.clone(), move |path: String, content: String| {
        write_file_with_log(&authority, &path, &content, &mut write_log.borrow_mut())
            .map_err(js_err)
    })?;
    fsobj.set("write", write_fn)?;

    host.set("fs", fsobj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("terrarium-fs-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// display path of a real dir, in the separator form the model would use
    fn display(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    /// planned-write authority whose only scope is a recursive prefix over `root`
    fn scoped_at(root: &Path) -> RunFilesystemAuthority {
        RunFilesystemAuthority::Scoped(
            vec![WriteScope::from_operator_spec(&display(root)).unwrap()],
        )
    }

    fn read_only() -> RunFilesystemAuthority {
        RunFilesystemAuthority::ReadOnly
    }

    #[test]
    fn list_returns_structured_entries() {
        let root = tmp_root("list-structured");
        std::fs::create_dir(root.join("dir")).unwrap();
        std::fs::write(root.join("file.txt"), "hello").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("file.txt"), root.join("link")).expect("symlink");
        let entries = list_dir(&display(&root)).unwrap();
        assert_eq!(entries[0].name, "dir");
        assert_eq!(entries[0].entry_type, "directory");
        assert_eq!(entries[0].size, None);
        assert_eq!(entries[1].name, "file.txt");
        assert_eq!(entries[1].entry_type, "file");
        assert_eq!(entries[1].size, Some(5));
        #[cfg(unix)]
        {
            assert_eq!(entries[2].name, "link");
            assert_eq!(entries[2].entry_type, "symlink");
            assert_eq!(entries[2].size, None);
        }
    }

    #[tokio::test]
    async fn list_serializes_entries_for_javascript() {
        let root = tmp_root("list-json");
        std::fs::create_dir(root.join("dir")).unwrap();
        std::fs::write(root.join("file.txt"), "hello").unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!("return host.fs.list('{}')", display(&root)),
            5_000,
            &read_only(),
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(
            out.value,
            Some(serde_json::json!([
                {"name": "dir", "type": "directory", "size": null},
                {"name": "file.txt", "type": "file", "size": 5}
            ]))
        );
    }

    #[test]
    fn glob_basics() {
        assert!(file_matches_glob("*.rs", "src/main.rs"));
        assert!(!file_matches_glob("*.rs", "src/main.ts"));
        assert!(!file_matches_glob("*.rs", "src/mod.rs.bak"));
        assert!(file_matches_glob("**/*.rs", "a/b/c.rs"));
        assert!(file_matches_glob("src/**/*.rs", "src/a/b/c.rs"));
        assert!(file_matches_glob("src/*.rs", "src/main.rs"));
        assert!(!file_matches_glob("src/*.rs", "src/a/main.rs")); // single * stays in-segment
        assert!(file_matches_glob("a?c.rs", "abc.rs"));
        assert!(!file_matches_glob("a?c.rs", "ac.rs"));
        assert!(!file_matches_glob("a?c.rs", "a/c.rs"));
        assert!(file_matches_glob("foo.spec.ts", "x/y/foo.spec.ts")); // no slash → basename
        assert!(file_matches_glob("**/*.rs", "main.rs"));
        assert!(file_matches_glob("src/**/*.rs", "src/main.rs"));
    }

    #[test]
    fn user_paths_reject_ambiguous_and_relative_forms() {
        // `/tmp/...` is not absolute on Windows; anchor the same lexical rules on the
        // platform's own prefix shape
        let base = if cfg!(windows) { "C:/tmp" } else { "/tmp" };
        assert!(validate_user_path(&format!("{base}/x.txt"), false).is_ok());
        assert!(validate_user_path(&format!("{base}/x.txt"), true).is_ok());
        assert!(validate_user_path("rel/x.txt", false).is_err());
        assert!(validate_user_path("~/x.txt", false).is_err());
        assert!(validate_user_path(&format!("{base}/../x.txt"), false).is_err());
        assert!(validate_user_path(&format!("{base}/./x.txt"), false).is_err());
        assert!(validate_user_path(&format!("{base}//x.txt"), false).is_err());
        #[cfg(not(windows))]
        assert!(validate_user_path(r"/tmp\x.txt", false).is_err());
        assert!(validate_user_path("", false).is_err());
        // exact-file rules: no trailing slash, no globs, not the root
        assert!(validate_user_path(&format!("{base}/dir/"), true).is_err());
        assert!(validate_user_path(&format!("{base}/*.txt"), true).is_err());
        assert!(validate_user_path(&format!("{base}/x?.txt"), true).is_err());
        assert!(validate_user_path(if cfg!(windows) { "C:/" } else { "/" }, true).is_err());
        // reads tolerate a trailing slash (directories are read roots)
        assert_eq!(
            validate_user_path(&format!("{base}/dir/"), false).unwrap(),
            PathBuf::from(format!("{base}/dir"))
        );
        // tildes and relative paths are named in the error the model sees
        let error = validate_user_path("~/x", false).unwrap_err();
        assert!(error.contains("~"), "{error}");
        let error = validate_user_path("x/y", false).unwrap_err();
        assert!(error.contains("absolute"), "{error}");
    }

    #[test]
    fn resolve_existing_folds_aliases_and_keeps_missing_tails() {
        let root = tmp_root("identity");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/f.txt"), "x").unwrap();
        let (base, tail) = resolve_existing(&root.join("a/f.txt")).unwrap();
        assert_eq!(base, root.join("a/f.txt").canonicalize().unwrap());
        assert!(tail.is_empty());
        let (base, tail) = resolve_existing(&root.join("a/new/deep.txt")).unwrap();
        assert_eq!(base, root.join("a").canonicalize().unwrap());
        assert_eq!(
            tail,
            vec![
                std::ffi::OsString::from("new"),
                std::ffi::OsString::from("deep.txt")
            ]
        );
        // a file where a directory is required cannot resolve
        assert!(resolve_existing(&root.join("a/f.txt/sub")).is_err());
    }

    #[test]
    fn operator_scopes_resolve_dirs_and_files_to_identities() {
        let root = tmp_root("scopes");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("f.txt"), "x").unwrap();
        let dir_scope = WriteScope::from_operator_spec(&display(&root.join("d"))).unwrap();
        let file_scope = WriteScope::from_operator_spec(&display(&root.join("f.txt"))).unwrap();
        // a DIR operand may carry the trailing separator temp-dir APIs hand out
        assert_eq!(
            WriteScope::from_operator_spec(&format!("{}/", display(&root.join("d")))).unwrap(),
            dir_scope
        );
        match (&dir_scope, &file_scope) {
            (WriteScope::Prefix(dir), WriteScope::Exact(file)) => {
                assert_eq!(*dir, root.join("d").canonicalize().unwrap());
                assert_eq!(*file, root.join("f.txt").canonicalize().unwrap());
            }
            _ => panic!("wrong scope kinds {dir_scope:?} {file_scope:?}"),
        }
        assert!(dir_scope.matches(&root.join("d").canonicalize().unwrap().join("nested/x.txt")));
        assert!(!dir_scope.matches(
            &root
                .join("outside.txt")
                .canonicalize()
                .unwrap_or(root.join("outside.txt"))
        ));
        assert!(file_scope.matches(&root.join("f.txt").canonicalize().unwrap()));
        assert!(!file_scope.matches(
            &root
                .join("g.txt")
                .canonicalize()
                .unwrap_or(root.join("g.txt"))
        ));
        assert!(WriteScope::from_operator_spec("/definitely/not/here").is_err());
        assert!(WriteScope::from_operator_spec("relative/path").is_err());
    }

    #[tokio::test]
    async fn scan_contains_filters_lines_before_the_js_boundary() {
        let root = tmp_root("scan-contains");
        std::fs::write(root.join("a.txt"), "keep one\nskip\nkeep two\n").unwrap();
        std::fs::write(root.join("b.txt"), "skip\n").unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!(
                "const lines = []; for await (const line of host.fs.scan('{}', {{contains: 'keep'}})) lines.push({{file: line.file, no: line.no, text: line.text}}); return lines;",
                display(&root)
            ),
            5_000,
            &read_only(),
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        let base = display(&root);
        assert_eq!(
            out.value,
            Some(serde_json::json!([
                {"file": format!("{base}/a.txt"), "no": 1, "text": "keep one"},
                {"file": format!("{base}/a.txt"), "no": 3, "text": "keep two"}
            ]))
        );
    }

    #[tokio::test]
    async fn scan_contains_continues_after_an_empty_nonfinal_batch() {
        let root = tmp_root("scan-contains-empty-batch");
        let content = format!("{}\nneedle\n", "x".repeat(SCAN_CHUNK_BYTES + 1));
        std::fs::write(root.join("a.txt"), content).unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!(
                "const lines = []; for await (const line of host.fs.scan('{}', {{contains: 'needle'}})) lines.push(line.text); return lines;",
                display(&root)
            ),
            5_000,
            &read_only(),
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(out.value, Some(serde_json::json!(["needle"])));
    }

    #[tokio::test]
    async fn walk_rejects_scan_only_contains_filter() {
        let root = tmp_root("walk-contains");
        std::fs::write(root.join("a.txt"), "keep\n").unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!("host.fs.walk('{}', {{contains: 'keep'}})", display(&root)),
            5_000,
            &read_only(),
            tx,
        )
        .await;
        assert!(!out.ok);
        let message = out.error.expect("walk option error").message;
        assert!(message.contains("contains"), "{message}");
        assert!(message.contains("scan"), "{message}");
    }

    #[tokio::test]
    async fn scan_rejects_invalid_option_types() {
        let root = tmp_root("scan-options");
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!("host.fs.scan('{}', {{hidden: 'yes'}})", display(&root)),
            5_000,
            &read_only(),
            tx,
        )
        .await;
        assert!(!out.ok);
        let message = out.error.expect("scan option error").message;
        assert!(message.contains("hidden"), "{message}");
    }

    #[test]
    fn scan_reports_invalid_utf8_instead_of_skipping_the_file() {
        let root = tmp_root("scan-utf8");
        std::fs::write(root.join("bad.txt"), b"ok\n\xff\n").unwrap();
        let mut state = scan_open(&display(&root), ScanOpts::default()).unwrap();
        let error = scan_next_chunk(&mut state).unwrap_err();
        assert!(error.contains("bad.txt"), "{error}");
        assert!(
            error.contains("UTF-8") || error.contains("utf-8"),
            "{error}"
        );
    }

    #[test]
    fn scan_streams_in_chunks_and_honors_glob_and_skips() {
        let root = tmp_root("scan");
        std::fs::create_dir_all(root.join("x/y")).unwrap();
        std::fs::create_dir_all(root.join("x/.git")).unwrap();
        std::fs::write(root.join("x/a.rs"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("x/c.txt"), "four\n").unwrap();
        std::fs::write(root.join("x/y/b.rs"), "three\n").unwrap();
        std::fs::write(root.join("x/.git/config"), "needle\n").unwrap();
        let base = display(&root.join("x"));

        let drains = |mut st: ScanState| {
            let mut all = Vec::new();
            loop {
                let c = scan_next_chunk(&mut st).unwrap();
                if c.is_empty() {
                    break;
                }
                all.extend(c);
            }
            all
        };
        // defaults (rg semantics): dot-entries skipped, .git never entered
        let all = drains(scan_open(&base, ScanOpts::default()).unwrap());
        let texts: Vec<&str> = all.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["one", "two", "four", "three"]);
        assert_eq!(all[0].file, format!("{base}/a.rs"));
        assert_eq!(all[0].no, 1);
        assert_eq!(all[2].file, format!("{base}/c.txt"));
        // glob filters candidates host-side (non-matching files are never opened)
        let rs = drains(
            scan_open(
                &base,
                ScanOpts {
                    glob: Some("*.rs".into()),
                    ..ScanOpts::default()
                },
            )
            .unwrap(),
        );
        let texts: Vec<&str> = rs.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["one", "two", "three"]);
        // {hidden: true} surfaces dot-entries (rg --hidden polarity)
        let all = drains(
            scan_open(
                &base,
                ScanOpts {
                    skip_hidden: false,
                    ..ScanOpts::default()
                },
            )
            .unwrap(),
        );
        assert!(all.iter().any(|l| l.file.ends_with(".git/config")));
        // skipDirs is an opt-in extra prune, not baked-in magic
        std::fs::create_dir_all(root.join("x/vendor")).unwrap();
        std::fs::write(root.join("x/vendor/v.rs"), "vend\n").unwrap();
        let all = drains(
            scan_open(
                &base,
                ScanOpts {
                    skip_dirs: vec!["vendor".into()],
                    ..ScanOpts::default()
                },
            )
            .unwrap(),
        );
        assert!(!all.iter().any(|l| l.file.contains("vendor")));
        // not-a-directory is a caller error
        let err = scan_open(&format!("{base}/a.rs"), ScanOpts::default()).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn walk_yields_files_with_sizes_and_scan_pruning_parity() {
        let root = tmp_root("walk");
        std::fs::create_dir_all(root.join("x/y")).unwrap();
        std::fs::create_dir_all(root.join("x/.git")).unwrap();
        std::fs::write(root.join("x/a.rs"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("x/c.txt"), "four\n").unwrap();
        std::fs::write(root.join("x/y/b.rs"), "three\n").unwrap();
        std::fs::write(root.join("x/.git/config"), "needle\n").unwrap();
        let base = display(&root.join("x"));

        let drain_walk = |mut st: ScanState| {
            let mut all = Vec::new();
            loop {
                let c = walk_next_chunk(&mut st).unwrap();
                if c.is_empty() {
                    break;
                }
                all.extend(c);
            }
            all
        };
        let drain_scan_files = |mut st: ScanState| {
            let mut files = Vec::new();
            loop {
                let c = scan_next_chunk(&mut st).unwrap();
                if c.is_empty() {
                    break;
                }
                for l in c {
                    if files.last() != Some(&l.file) {
                        files.push(l.file);
                    }
                }
            }
            files
        };

        // defaults: same file set as scan, in the same deterministic order, with sizes
        let walked: Vec<(String, u64)> = drain_walk(walk_open(&base, ScanOpts::default()).unwrap())
            .into_iter()
            .map(|e| (e.file, e.size))
            .collect();
        assert_eq!(
            walked,
            vec![
                (format!("{base}/a.rs"), 8),
                (format!("{base}/c.txt"), 5),
                (format!("{base}/y/b.rs"), 6),
            ]
        );
        let scanned = drain_scan_files(scan_open(&base, ScanOpts::default()).unwrap());
        assert_eq!(
            walked.iter().map(|(f, _)| f.clone()).collect::<Vec<_>>(),
            scanned,
            "walk and scan must prune identically"
        );

        // glob filters host-side: non-matching files are never touched
        let rs: Vec<String> = drain_walk(
            walk_open(
                &base,
                ScanOpts {
                    glob: Some("*.rs".into()),
                    ..ScanOpts::default()
                },
            )
            .unwrap(),
        )
        .into_iter()
        .map(|e| e.file)
        .collect();
        assert_eq!(rs, [format!("{base}/a.rs"), format!("{base}/y/b.rs")]);

        // {hidden: true} surfaces dot-entries (rg --hidden polarity), parity with scan
        let with_hidden = drain_walk(
            walk_open(
                &base,
                ScanOpts {
                    skip_hidden: false,
                    ..ScanOpts::default()
                },
            )
            .unwrap(),
        );
        assert!(with_hidden.iter().any(|e| e.file.ends_with(".git/config")));

        // skipDirs is the same opt-in extra prune
        std::fs::create_dir_all(root.join("x/vendor")).unwrap();
        std::fs::write(root.join("x/vendor/v.rs"), "vend\n").unwrap();
        let pruned = drain_walk(
            walk_open(
                &base,
                ScanOpts {
                    skip_dirs: vec!["vendor".into()],
                    ..ScanOpts::default()
                },
            )
            .unwrap(),
        );
        assert!(!pruned.iter().any(|e| e.file.contains("vendor")));

        // walk never opens files: a NUL binary counts as an entry even though scan
        // (a text channel) never yields its lines — the layering, made observable
        std::fs::write(root.join("x/bin.dat"), b"\x00\x01binary").unwrap();
        let w = drain_walk(walk_open(&base, ScanOpts::default()).unwrap());
        assert!(w.iter().any(|e| e.file.ends_with("bin.dat")));
        let s = drain_scan_files(scan_open(&base, ScanOpts::default()).unwrap());
        assert!(!s.iter().any(|f| f.ends_with("bin.dat")));

        // not-a-directory is the same caller error as scan
        let err = walk_open(&format!("{base}/a.rs"), ScanOpts::default()).unwrap_err();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[tokio::test]
    async fn walk_streams_through_the_prelude_bridge() {
        let root = tmp_root("walk-js");
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join("d/a.rs"), "x\n").unwrap();
        std::fs::write(root.join("d/b.txt"), "y\n").unwrap();
        std::fs::write(root.join("d/c.rs"), "z\n").unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!(
                "let n = 0, bytes = 0;\nfor await (const f of host.fs.walk('{}', {{glob: '*.rs'}})) {{ n++; bytes += f.size; }}\nreturn {{n, bytes}};",
                display(&root.join("d"))
            ),
            5_000,
            &read_only(),
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(out.value, Some(serde_json::json!({"n": 2, "bytes": 4})));
    }

    #[test]
    fn gitignore_rules_prune_the_walk_and_can_be_overridden() {
        let root = tmp_root("gi");
        // covers the whole subset: anchored dir-only, basename anywhere, **, negation, nested scope
        std::fs::write(
            root.join(".gitignore"),
            "/out/\n*.log\n!important.log\nbuild/\n**/*.gen.rs\n",
        )
        .unwrap();
        std::fs::write(root.join("keep.rs"), "k\n").unwrap();
        std::fs::write(root.join("a.log"), "x\n").unwrap();
        std::fs::write(root.join("important.log"), "i\n").unwrap(); // ! negation re-includes
        std::fs::create_dir_all(root.join("out/deep")).unwrap();
        std::fs::write(root.join("out/skipme.rs"), "x\n").unwrap(); // anchored dir-only
        std::fs::create_dir_all(root.join("src/build")).unwrap();
        std::fs::write(root.join("src/build/s.rs"), "x\n").unwrap(); // unanchored dir pattern
        std::fs::write(root.join("src/w.gen.rs"), "x\n").unwrap(); // ** anywhere
        std::fs::write(root.join("src/ok.rs"), "o\n").unwrap();
        // nested scope: only ignores vend/ below sub/
        std::fs::create_dir_all(root.join("sub/vend")).unwrap();
        std::fs::write(root.join("sub/.gitignore"), "vend/\n").unwrap();
        std::fs::write(root.join("sub/vend/v.rs"), "x\n").unwrap(); // ignored by nested scope
        std::fs::create_dir_all(root.join("vend")).unwrap();
        std::fs::write(root.join("vend/root.rs"), "r\n").unwrap(); // NOT covered by sub/.gitignore
        let base = display(&root);
        let drain = |opts: ScanOpts| {
            let mut st = scan_open(&base, opts).unwrap();
            let mut v = Vec::new();
            loop {
                let c = scan_next_chunk(&mut st).unwrap();
                if c.is_empty() {
                    break;
                }
                v.extend(c.into_iter().map(|l| l.file));
            }
            v
        };
        let on = drain(ScanOpts::default());
        let mut uniq = on.clone();
        uniq.dedup();
        assert_eq!(uniq.len(), 4, "{uniq:?}"); // the 4 non-hidden survivors
        assert!(uniq.contains(&format!("{base}/keep.rs")));
        assert!(uniq.contains(&format!("{base}/important.log"))); // negation won
        assert!(uniq.contains(&format!("{base}/src/ok.rs")));
        assert!(uniq.contains(&format!("{base}/vend/root.rs"))); // nested scope stays nested
        assert!(!on.iter().any(|f| f.contains("a.log")));
        assert!(!on.iter().any(|f| f.contains("/out/")));
        assert!(!on.iter().any(|f| f.contains("build/")));
        assert!(!on.iter().any(|f| f.ends_with("w.gen.rs")));
        assert!(!on.iter().any(|f| f.contains("sub/vend/")));
        // rule files are dot-entries — skipped by default like rg, present under {hidden: true}
        assert!(!on.iter().any(|f| f.ends_with(".gitignore")));
        let seen = drain(ScanOpts {
            skip_hidden: false,
            ..ScanOpts::default()
        });
        assert!(seen.iter().any(|f| f == &format!("{base}/.gitignore")));
        assert!(seen.iter().any(|f| f == &format!("{base}/sub/.gitignore")));
        // the model's escape hatch: everything non-hidden comes back
        let off = drain(ScanOpts {
            gitignore: false,
            ..ScanOpts::default()
        });
        assert!(off.iter().any(|f| f.ends_with("a.log")));
        assert!(off.iter().any(|f| f.contains("/out/skipme.rs")));
        assert!(off.iter().any(|f| f.contains("src/build/")));
        assert!(off.iter().any(|f| f.contains("sub/vend/")));
    }

    #[test]
    fn scan_skips_nul_binaries_and_caps_pathological_lines() {
        let root = tmp_root("scanbin");
        // NUL-containing file with an ALLOWED extension: never surfaces (scan is a text channel)
        std::fs::write(
            root.join("movie.mp4"),
            b"\x00\x00\x00\x1cftypisom needle-in-binary\x00",
        )
        .unwrap();
        std::fs::write(root.join("ok.txt"), "fine\n").unwrap();
        // minified-style single line (200KB, no newline until the end) followed by a normal line:
        // delivered WHOLE — match completeness belongs to the predicate, not the transport
        let long = format!("{}tail\nafter\n", "x".repeat(200 * 1024));
        std::fs::write(root.join("mini.json"), long).unwrap();
        let mut st = scan_open(&display(&root), ScanOpts::default()).unwrap();
        let mut all = Vec::new();
        loop {
            let c = scan_next_chunk(&mut st).unwrap();
            if c.is_empty() {
                break;
            }
            all.extend(c);
        }
        let files: Vec<&str> = all
            .iter()
            .map(|l| l.file.rsplit('/').next().unwrap())
            .collect();
        assert!(!files.contains(&"movie.mp4"), "{files:?}");
        let mini: Vec<&ScanLine> = all
            .iter()
            .filter(|l| l.file.ends_with("mini.json"))
            .collect();
        assert_eq!(mini.len(), 2);
        assert_eq!(mini[0].text.chars().count(), 200 * 1024 + 4); // 'x'*200KB + "tail", nothing cut
        assert!(mini[0].text.ends_with("tail"));
        assert_eq!(mini[1].text, "after");
        assert_eq!(mini[1].no, 2);
        assert!(all.iter().any(|l| l.text == "fine"));
    }

    #[test]
    fn scan_hard_cuts_monster_lines_beyond_the_heap_scale() {
        let root = tmp_root("scanmonster");
        // a single 9MB line: past the 8MB read bound the line is cut (marked '…') and its
        // remainder drained — the bound ≈ what the 64MB cage heap could hold anyway
        let mut blob = String::with_capacity(9 * 1024 * 1024 + 8);
        for _ in 0..9 * 1024 * 1024 {
            blob.push('x');
        }
        blob.push_str("needle\n");
        std::fs::write(root.join("blob.txt"), blob.as_bytes()).unwrap();
        std::fs::write(root.join("after.txt"), "still scanned\n").unwrap();
        let mut st = scan_open(&display(&root), ScanOpts::default()).unwrap();
        let mut all = Vec::new();
        loop {
            let c = scan_next_chunk(&mut st).unwrap();
            if c.is_empty() {
                break;
            }
            all.extend(c);
        }
        let blob_lines: Vec<&ScanLine> = all
            .iter()
            .filter(|l| l.file.ends_with("blob.txt"))
            .collect();
        assert_eq!(blob_lines.len(), 1);
        assert_eq!(blob_lines[0].text.len(), (SCAN_LINE_HARD + 1) as usize + 3); // take-limit bytes + '…' (3 bytes UTF-8)
        assert!(blob_lines[0].text.ends_with('…'));
        assert!(!blob_lines[0].text.contains("needle")); // beyond the cut — documented incompleteness
                                                         // the drain ended exactly at the line's end: the next file still scans with correct numbering
        let after: Vec<&ScanLine> = all
            .iter()
            .filter(|l| l.file.ends_with("after.txt"))
            .collect();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].text, "still scanned");
    }

    #[test]
    fn write_is_denied_in_read_only_mode() {
        let root = tmp_root("ro");
        let err = write_file(&read_only(), &display(&root.join("new.txt")), "hi").unwrap_err();
        assert!(
            err.contains("write_denied")
                && err.contains("read-only")
                && err.contains("Policy denial"),
            "{err}"
        );
        assert!(!root.join("new.txt").exists());
    }

    #[test]
    fn write_is_atomic_creates_parents_and_round_trips() {
        let root = tmp_root("rw");
        let authority = scoped_at(&root);
        let mut writes = WriteLog::default();
        let target = display(&root.join("a/b/c.txt"));
        let n = write_file_with_log(&authority, &target, "hello\nworld", &mut writes).unwrap();
        assert_eq!(n, 11);
        assert_eq!(writes.items.len(), 1);
        assert_eq!(writes.items[0].path, target);
        assert!(writes.items[0].created);
        assert!(writes.items[0].changed);
        assert_eq!(writes.items[0].bytes_before, None);
        assert_eq!(writes.items[0].bytes_after, 11);
        assert_eq!(writes.items[0].first_changed_line, Some(1));
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c.txt")).unwrap(),
            "hello\nworld"
        );
        // no temp litter after a successful write
        assert!(std::fs::read_dir(root.join("a/b")).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".terrarium-")));
        // overwrite is a rename, not a truncate-in-place
        write_file_with_log(&authority, &target, "x", &mut writes).unwrap();
        assert_eq!(writes.items.len(), 2);
        assert!(!writes.items[1].created);
        assert_eq!(writes.items[1].bytes_before, Some(11));
        assert_eq!(writes.items[1].bytes_after, 1);
        assert_eq!(writes.items[1].first_changed_line, Some(1));
        write_file_with_log(&authority, &target, "x", &mut writes).unwrap();
        assert_eq!(writes.items.len(), 3);
        assert!(!writes.items[2].created);
        assert!(!writes.items[2].changed);
        assert_eq!(writes.items[2].bytes_before, Some(1));
        assert_eq!(writes.items[2].bytes_after, 1);
        assert_eq!(writes.items[2].first_changed_line, None);
        assert_eq!(
            std::fs::read_to_string(root.join("a/b/c.txt")).unwrap(),
            "x"
        );
    }

    #[test]
    fn undeclared_targets_are_rejected_with_write_not_authorized() {
        let root = tmp_root("scoped");
        let authority = scoped_at(&root);
        // inside the operator prefix: allowed
        assert!(write_file(&authority, &display(&root.join("in.txt")), "x").is_ok());
        // outside: write_not_authorized, nothing created
        let outside = tmp_root("scoped-outside");
        let err = write_file(&authority, &display(&outside.join("out.txt")), "x").unwrap_err();
        assert!(
            err.contains("write_not_authorized") && err.contains("access block"),
            "{err}"
        );
        assert!(!outside.join("out.txt").exists());
        // an exact-file scope covers exactly one file
        let exact_root = tmp_root("scoped-exact");
        std::fs::write(exact_root.join("one.txt"), "1").unwrap();
        let exact = RunFilesystemAuthority::Scoped(vec![WriteScope::from_operator_spec(&display(
            &exact_root.join("one.txt"),
        ))
        .unwrap()]);
        assert!(write_file(&exact, &display(&exact_root.join("one.txt")), "2").is_ok());
        let err = write_file(&exact, &display(&exact_root.join("two.txt")), "x").unwrap_err();
        assert!(err.contains("write_not_authorized"), "{err}");
        assert!(!exact_root.join("two.txt").exists());
        // empty scope denies everything but stays planned-write shaped
        let empty = RunFilesystemAuthority::Scoped(Vec::new());
        let err = write_file(&empty, &display(&root.join("denied.txt")), "x").unwrap_err();
        assert!(err.contains("write_not_authorized"), "{err}");
    }

    #[test]
    fn lexical_escapes_are_rejected_before_any_filesystem_access() {
        let root = tmp_root("esc");
        let authority = scoped_at(&root);
        for spec in [
            format!("{}/../escape.txt", display(&root).trim_end_matches('/')),
            format!("{}/./x.txt", display(&root).trim_end_matches('/')),
            format!("{}/..", display(&root).trim_end_matches('/')),
        ] {
            let err = write_file(&authority, &spec, "x").unwrap_err();
            assert!(err.contains("..") || err.contains("`.`"), "{spec} -> {err}");
        }
        assert!(!root.parent().unwrap().join("escape.txt").exists());
    }

    #[test]
    fn read_window_footer_iff_more_lines() {
        let root = tmp_root("read");
        std::fs::write(root.join("f.txt"), "one\ntwo\nthree\nfour").unwrap();
        let file = display(&root.join("f.txt"));
        let cut = read_window(&file, 1, 2).unwrap();
        assert_eq!(
            cut,
            format!(
                "1: one\n2: two\n[more lines follow — continue with host.fs.read(\"{file}\", 3, …)]"
            )
        );
        let whole = read_window(&file, 1, usize::MAX).unwrap();
        assert!(
            whole.ends_with("4: four") && !whole.contains("more lines"),
            "{whole}"
        );
        let mid = read_window(&file, 2, 3).unwrap();
        assert_eq!(mid, format!("2: two\n3: three\n[more lines follow — continue with host.fs.read(\"{file}\", 4, …)]"));
    }

    #[test]
    fn read_caps_pathological_lines() {
        let root = tmp_root("longline");
        std::fs::write(root.join("min.txt"), format!("{}\nend", "x".repeat(5000))).unwrap();
        let s = read_window(&display(&root.join("min.txt")), 1, 2).unwrap();
        let l1 = s.lines().next().unwrap();
        assert!(
            l1.starts_with("1: ") && l1.ends_with('…') && l1.chars().count() <= 2004,
            "{l1:?}"
        );
    }

    #[test]
    fn write_restores_crlf_bom_and_lone_cr() {
        let root = tmp_root("shape");
        let authority = scoped_at(&root);
        // CRLF-dominant + BOM target: round-trip restores both; return value counts real bytes written
        std::fs::write(root.join("win.txt"), b"\xEF\xBB\xBFa\r\nb\r\nc\n").unwrap();
        let n = write_file(&authority, &display(&root.join("win.txt")), "x\ny").unwrap();
        assert_eq!(
            std::fs::read(root.join("win.txt")).unwrap(),
            b"\xEF\xBB\xBFx\r\ny"
        );
        assert_eq!(n, 3 + 3 + 1); // BOM + "x\r\n" + "y"
                                  // LF target stays LF; lone \r survives untouched either way
        std::fs::write(root.join("unix.txt"), "a\nb\n").unwrap();
        write_file(&authority, &display(&root.join("unix.txt")), "x\ry\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("unix.txt")).unwrap(),
            "x\ry\n"
        );
        // a brand-new file gets exactly what was given — no BOM, no CRLF invention
        write_file(&authority, &display(&root.join("fresh.txt")), "1\n2").unwrap();
        assert_eq!(std::fs::read(root.join("fresh.txt")).unwrap(), b"1\n2");
        // majority rules within the 4KB sample: CRLF wins ties only on strict majority
        std::fs::write(root.join("mix.txt"), "a\r\nb\nc\n").unwrap(); // crlf=1, lf=1 → LF
        write_file(&authority, &display(&root.join("mix.txt")), "z\n").unwrap();
        assert_eq!(std::fs::read(root.join("mix.txt")).unwrap(), b"z\n");
    }

    #[test]
    fn text_channel_is_lf_normalized_and_round_trips() {
        let root = tmp_root("textch");
        let authority = scoped_at(&root);
        std::fs::write(root.join("c.txt"), "a\r\nb\r\nc").unwrap();
        assert_eq!(read_text(&display(&root.join("c.txt"))).unwrap(), "a\nb\nc");
        // surgical idiom: text → replace → write restores the file's own EOL
        let edited = read_text(&display(&root.join("c.txt")))
            .unwrap()
            .replace("b", "B");
        write_file(&authority, &display(&root.join("c.txt")), &edited).unwrap();
        assert_eq!(std::fs::read(root.join("c.txt")).unwrap(), b"a\r\nB\r\nc");
    }

    #[tokio::test]
    async fn host_fs_replace_makes_one_exact_change_and_returns_a_small_receipt() {
        let root = tmp_root("edit-replace");
        std::fs::write(root.join("file.txt"), "one\ntwo\nthree\n").unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!(
                "return host.fs.replace('{}', 'two', 'TWO')",
                display(&root.join("file.txt"))
            ),
            5_000,
            &scoped_at(&root),
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(
            out.value,
            Some(serde_json::json!({
                "path": display(&root.join("file.txt")),
                "replacements": 1,
                "bytes": 14
            }))
        );
        assert_eq!(out.writes.len(), 1);
        assert_eq!(out.writes[0].path, display(&root.join("file.txt")));
        assert!(!out.writes[0].created);
        assert!(out.writes[0].changed);
        assert_eq!(out.writes[0].bytes_before, Some(14));
        assert_eq!(out.writes[0].bytes_after, 14);
        assert_eq!(out.writes[0].first_changed_line, Some(2));
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
    }

    #[tokio::test]
    async fn replace_enforces_the_runs_frozen_authority() {
        let root = tmp_root("edit-auth");
        std::fs::write(root.join("file.txt"), "one\ntwo\n").unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        // read-only invocation: the replace fails at the underlying write
        let out = crate::kernel::eval_js(
            &format!(
                "return host.fs.replace('{}', 'two', 'TWO')",
                display(&root.join("file.txt"))
            ),
            5_000,
            &read_only(),
            tx,
        )
        .await;
        assert!(!out.ok);
        let message = out.error.expect("denied").message;
        assert!(message.contains("write_denied"), "{message}");
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "one\ntwo\n"
        );
    }

    #[tokio::test]
    async fn write_receipt_survives_a_later_program_failure() {
        let root = tmp_root("write-failure");
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!(
                "host.fs.write('{}', 'written'); throw new Error('after write')",
                display(&root.join("file.txt"))
            ),
            5_000,
            &scoped_at(&root),
            tx,
        )
        .await;
        assert!(!out.ok);
        assert_eq!(out.writes.len(), 1);
        assert!(out.writes[0].created);
        assert_eq!(out.writes[0].bytes_after, 7);
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "written"
        );
    }

    #[tokio::test]
    async fn host_fs_replace_rejects_missing_ambiguous_and_noop_edits_without_writing() {
        let root = tmp_root("edit-reject");
        let original = "same\nmiddle\nsame\n";
        std::fs::write(root.join("file.txt"), original).unwrap();
        let authority = scoped_at(&root);

        for (source, expected) in [
            (
                format!(
                    "return host.fs.replace('{}', 'missing', 'new')",
                    display(&root.join("file.txt"))
                ),
                "not found",
            ),
            (
                format!(
                    "return host.fs.replace('{}', 'same', 'new')",
                    display(&root.join("file.txt"))
                ),
                "matched 2 times",
            ),
            (
                format!(
                    "return host.fs.replace('{}', 'middle', 'middle')",
                    display(&root.join("file.txt"))
                ),
                "identical",
            ),
        ] {
            let (tx, _rx) = tokio::sync::watch::channel(false);
            let out = crate::kernel::eval_js(&source, 5_000, &authority, tx).await;
            assert!(!out.ok, "source unexpectedly succeeded: {source}");
            let message = out.error.expect("edit error").message;
            assert!(message.contains(expected), "{message}");
            assert_eq!(
                std::fs::read_to_string(root.join("file.txt")).unwrap(),
                original
            );
        }
    }

    #[tokio::test]
    async fn host_fs_replace_treats_replacement_text_literally() {
        let root = tmp_root("edit-literal");
        std::fs::write(root.join("file.txt"), "const marker = OLD;\n").unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!(
                "return host.fs.replace('{}', 'OLD', '$&')",
                display(&root.join("file.txt"))
            ),
            5_000,
            &scoped_at(&root),
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "const marker = $&;\n"
        );
    }

    #[tokio::test]
    async fn host_fs_replace_all_is_explicit() {
        let root = tmp_root("edit-all");
        std::fs::write(root.join("file.txt"), "x x x").unwrap();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let out = crate::kernel::eval_js(
            &format!(
                "return host.fs.replace('{}', 'x', 'y', {{all: true}})",
                display(&root.join("file.txt"))
            ),
            5_000,
            &scoped_at(&root),
            tx,
        )
        .await;
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(out.value.as_ref().unwrap()["replacements"], 3);
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "y y y"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reads_follow_the_os_view_while_writes_reject_symlinks() {
        let root = tmp_root("symlink");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("secret.txt"), "top secret").unwrap();
        let outside = tmp_root("symlink-out");
        std::fs::write(outside.join("private.txt"), "private").unwrap();
        // reads use the OS user's readable view: an outward symlink is readable
        std::os::unix::fs::symlink(outside.join("private.txt"), root.join("leak.txt")).unwrap();
        std::os::unix::fs::symlink(root.join("secret.txt"), root.join("alias.txt")).unwrap();
        assert_eq!(
            read_text(&display(&root.join("leak.txt"))).unwrap(),
            "private"
        );
        assert_eq!(
            read_text(&display(&root.join("alias.txt"))).unwrap(),
            "top secret"
        );
        // writes never target an existing symlink, in or out of scope
        let authority = scoped_at(&root);
        let err = write_file(&authority, &display(&root.join("leak.txt")), "x").unwrap_err();
        assert!(err.contains("symbolic link"), "{err}");
        let err = write_file(&authority, &display(&root.join("alias.txt")), "x").unwrap_err();
        assert!(err.contains("symbolic link"), "{err}");
        assert_eq!(
            std::fs::read_to_string(outside.join("private.txt")).unwrap(),
            "private"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("secret.txt")).unwrap(),
            "top secret"
        );
        // scan never follows symlinks during traversal (rg default without -L)
        std::os::unix::fs::symlink(&outside, root.join("sub/evil")).unwrap();
        let mut st = scan_open(&display(&root), ScanOpts::default()).unwrap();
        let mut files = Vec::new();
        loop {
            let c = scan_next_chunk(&mut st).unwrap();
            if c.is_empty() {
                break;
            }
            files.extend(c.into_iter().map(|l| l.file));
        }
        assert!(!files.iter().any(|f| f.contains("private")), "{files:?}");
        assert!(!files.iter().any(|f| f.contains("leak")), "{files:?}");
        assert!(!files.iter().any(|f| f.contains("alias")), "{files:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_scan_root_reads_through_the_link() {
        let root = tmp_root("symlink-root");
        let outside = tmp_root("symlink-root-out");
        std::fs::write(outside.join("visible.txt"), "v\n").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("rootlink")).unwrap();
        let mut st = scan_open(&display(&root.join("rootlink")), ScanOpts::default()).unwrap();
        let mut files = Vec::new();
        loop {
            let c = scan_next_chunk(&mut st).unwrap();
            if c.is_empty() {
                break;
            }
            files.extend(c.into_iter().map(|l| l.file));
        }
        assert!(
            files.iter().any(|f| f.ends_with("visible.txt")),
            "{files:?}"
        );
    }

    #[test]
    fn text_channel_rejects_files_beyond_the_cage_heap() {
        let root = tmp_root("huge");
        let f = std::fs::File::create(root.join("huge.bin")).unwrap();
        f.set_len(crate::MEM_LIMIT as u64 + 1).unwrap(); // sparse: no bytes actually written
        let err = read_text(&display(&root.join("huge.bin"))).unwrap_err();
        assert!(err.contains("64MB cage heap"), "{err}");
    }

    #[test]
    fn text_channel_strips_a_leading_bom() {
        let root = tmp_root("bom");
        std::fs::write(root.join("b.txt"), b"\xEF\xBB\xBFa\r\nb").unwrap();
        assert_eq!(read_text(&display(&root.join("b.txt"))).unwrap(), "a\nb");
        // write restores what the target had: the round trip is shape-preserving both ways
        let authority = scoped_at(&root);
        write_file(&authority, &display(&root.join("b.txt")), "c\nd").unwrap();
        assert_eq!(
            std::fs::read(root.join("b.txt")).unwrap(),
            b"\xEF\xBB\xBFc\r\nd"
        );
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn alternate_case_spellings_collapse_to_one_identity() {
        let root = tmp_root("case-spelling");
        let prefix = display(&root);
        let alternate: String = prefix
            .chars()
            .map(|ch| {
                if ch.is_ascii_lowercase() {
                    ch.to_ascii_uppercase()
                } else if ch.is_ascii_uppercase() {
                    ch.to_ascii_lowercase()
                } else {
                    ch
                }
            })
            .collect();
        if alternate == prefix || std::path::Path::new(&alternate).canonicalize().is_err() {
            return;
        }
        std::fs::write(root.join("file.txt"), "case-insensitive").unwrap();
        assert_eq!(
            read_text(&format!("{alternate}/file.txt")).unwrap(),
            "case-insensitive"
        );
        // write scope membership also collapses the spelling
        let authority = scoped_at(&root);
        assert!(
            write_file(&authority, &format!("{alternate}/file.txt"), "ok").is_ok(),
            "scope membership must be case-insensitive on this platform"
        );
    }

    #[test]
    fn glob_terminates_on_pathological_patterns() {
        // exponential backtracking bait: with the old recursive matcher this hung a whole poll
        let name: String = "a".repeat(40);
        assert!(!file_matches_glob("*a*a*a*a*a*a*a*a*a*b", &name));
        assert!(file_matches_glob(&format!("*{}", "a".repeat(40)), &name));
        // deep `**` chains must not recurse (multi-MB patterns once overflowed the native stack)
        let deep: Vec<char> = "*".repeat(2000).chars().collect();
        assert!(glob_match(&deep, &"x/y/z".chars().collect::<Vec<_>>()));
    }
}
