//! Process execution: run-scoped `exec` and session-scoped `spawn` over a host-owned
//! in-memory process table. A process is host-owned session state referenced by an opaque
//! handle, exactly as a file is referenced by a path. Security lives here: the frozen
//! command authority is checked at every call (resolved executable, element-wise argv,
//! resolved cwd), children run in their own process group (Unix) or a kill-on-close job
//! object (Windows), and the run deadline bounds the observer — never the observed. The
//! journal receives receipts (`run/spawn`, `proc/exit`), never stream data; spawn output
//! has exactly one durable home, a host-owned append-only log file read with `host.fs.read`.

use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rquickjs::function::{Async, Opt};
use rquickjs::{Ctx, Function, Object, Value};
use serde_json::json;
use tokio::sync::watch;

/// Request bound from the authorization contract: at most 8 command records.
pub(crate) const MAX_COMMANDS: usize = 8;
/// One exec stream (stdout, stderr) is bounded to 16 KiB as head-plus-tail.
const CAPTURE_STREAM: usize = 16 * 1024;
const CAPTURE_EDGE: usize = CAPTURE_STREAM / 2;
/// A spawn log stops appending at 4 MiB and gains one final marker line.
pub(crate) const LOG_CAP: u64 = 4 * 1024 * 1024;
/// The session table holds at most 8 live processes and 16 entries total.
pub(crate) const MAX_LIVE: usize = 8;
pub(crate) const MAX_ENTRIES: usize = 16;
/// The `proc/exit` receipt tail stays near 1 KiB.
pub(crate) const EXIT_TAIL_CAP: usize = 1024;
/// Pending exit receipts are forensics, bounded like every other queue.
const EXIT_QUEUE_CAP: usize = 256;

// ---------------------------------------------------------------------------
// Authority
// ---------------------------------------------------------------------------

/// One approved command, fully resolved. Matching at call time is resolved-identity
/// equality plus element-wise argv equality plus cwd equality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    pub exe: PathBuf,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

/// The approved command set: exact records from the access block, plus executable-only
/// operator pre-grants (`--allow-exec`) that match any argv.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandSet {
    pub grants: Vec<PathBuf>,
    pub records: Vec<CommandRecord>,
}

impl CommandSet {
    fn describe(&self) -> String {
        let mut parts: Vec<String> = self
            .records
            .iter()
            .map(|r| {
                format!(
                    "{{\"exe\": {}, \"argv\": {}, \"cwd\": {}}}",
                    serde_json::to_string(&r.exe.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default(),
                    serde_json::to_string(&r.argv).unwrap_or_default(),
                    serde_json::to_string(&r.cwd.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default(),
                )
            })
            .collect();
        parts.extend(self.grants.iter().map(|exe| {
            format!(
                "any command of {}",
                exe.to_string_lossy().replace('\\', "/")
            )
        }));
        if parts.is_empty() {
            "no commands are authorized for this run; declare the exact command in the \
                ```access block"
                .into()
        } else {
            parts.join("; ")
        }
    }
}

/// The frozen per-run process authority, decided before QuickJS starts. `Allowed` is the
/// planned-write shape (operator pre-grants plus approved declarations); `Unrestricted`
/// is full-access (no prompt, still journaled); `Denied` covers read-only and empty sets.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ProcAuthority {
    #[default]
    Denied,
    Allowed(CommandSet),
    Unrestricted,
}

/// Resolve an executable the way the host does: bare names go through PATH lookup,
/// separator-bearing spellings must be absolute; identity is the canonicalized path so
/// symlink normalization cannot smuggle two spellings of one program past the check.
pub(crate) fn resolve_executable(exe: &str) -> Result<PathBuf, String> {
    if exe.is_empty() {
        return Err("exe must be a non-empty executable name or absolute path".into());
    }
    if exe.contains('/') || (cfg!(windows) && exe.contains('\\')) {
        let path = Path::new(exe);
        if !path.is_absolute() {
            return Err(format!(
                "exe {exe:?} contains a path separator; pass either a bare name (resolved on \
                 PATH) or an absolute path"
            ));
        }
        return canonical_exe(path, exe);
    }
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_env) {
        for candidate in exe_spellings(exe) {
            let full = dir.join(&candidate);
            if is_executable_file(&full) {
                return full.canonicalize().map_err(|e| format!("exe {exe:?}: {e}"));
            }
        }
    }
    Err(format!("exe {exe:?} was not found on PATH"))
}

fn canonical_exe(path: &Path, display: &str) -> Result<PathBuf, String> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Err(format!("exe {display:?} is a directory")),
        Ok(_) => path
            .canonicalize()
            .map_err(|e| format!("exe {display:?}: {e}")),
        Err(e) => Err(format!("exe {display:?}: {e}")),
    }
}

fn exe_spellings(name: &str) -> Vec<String> {
    if cfg!(windows) {
        vec![
            name.to_string(),
            format!("{name}.exe"),
            format!("{name}.cmd"),
            format!("{name}.bat"),
            format!("{name}.com"),
        ]
    } else {
        vec![name.to_string()]
    }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Resolve a command's working directory: the invocation working root by default, or an
/// absolute existing directory canonicalized for identity.
pub(crate) fn resolve_cwd(cwd: Option<&str>, working_root: &Path) -> Result<PathBuf, String> {
    let Some(raw) = cwd else {
        return Ok(working_root.to_path_buf());
    };
    let path =
        crate::fs::validate_user_path(raw, false).map_err(|e| format!("cwd {raw:?}: {e}"))?;
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_dir() => path.canonicalize().map_err(|e| format!("cwd {raw:?}: {e}")),
        Ok(_) => Err(format!("cwd {raw:?} is not a directory")),
        Err(e) => Err(format!("cwd {raw:?}: {e}")),
    }
}

impl ProcAuthority {
    /// Check one call against the frozen records, resolving exe and cwd again so the
    /// approval and the launch see the same program. A mismatch is the corrective
    /// `command_not_authorized` error carrying the full expected records.
    pub(crate) fn authorize(
        &self,
        exe: &str,
        argv: &[String],
        cwd: Option<&str>,
        working_root: &Path,
    ) -> Result<(PathBuf, PathBuf), String> {
        let resolved_exe = resolve_executable(exe)?;
        let resolved_cwd = resolve_cwd(cwd, working_root)?;
        let allowed = match self {
            ProcAuthority::Unrestricted => true,
            ProcAuthority::Allowed(set) => {
                set.grants.contains(&resolved_exe)
                    || set.records.iter().any(|record| {
                        record.exe == resolved_exe
                            && record.argv == argv
                            && record.cwd == resolved_cwd
                    })
            }
            ProcAuthority::Denied => false,
        };
        if !allowed {
            let expected = match self {
                ProcAuthority::Allowed(set) => set.describe(),
                _ => "process creation is denied in this invocation".into(),
            };
            return Err(format!(
                "command_not_authorized: {{\"exe\": {:?}, \"argv\": {}, \"cwd\": {:?}}} matches \
                 no authorized command; expected: {expected}",
                exe,
                serde_json::to_string(argv).unwrap_or_default(),
                cwd.unwrap_or_default(),
            ));
        }
        Ok((resolved_exe, resolved_cwd))
    }
}

// ---------------------------------------------------------------------------
// Platform: process groups, death signals, job objects, termination
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn prepare_command(cmd: &mut tokio::process::Command) {
    // the child leads its own group so kill(id) reaches the whole tree it builds
    let _ = cmd.process_group(0);
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
}

#[cfg(windows)]
fn prepare_command(_cmd: &mut tokio::process::Command) {}

/// How to terminate one spawned process tree. Unix uses the process group; Windows uses
/// a kill-on-close job object assigned right after spawn.
#[derive(Debug)]
enum ChildKill {
    #[cfg(unix)]
    Group(u32),
    #[cfg(windows)]
    Job(windows_sys::Win32::Foundation::HANDLE),
}

// A HANDLE is an opaque kernel-object reference; job-object calls are thread-safe and
// the guard discipline gives one owner, so moving it across threads is sound. Without
// this, the raw pointer would make every table entry (and the pump future) !Send.
#[cfg(windows)]
unsafe impl Send for ChildKill {}
#[cfg(windows)]
unsafe impl Sync for ChildKill {}

impl ChildKill {
    #[cfg(unix)]
    fn for_child(pid: u32) -> Result<Self, String> {
        Ok(Self::Group(pid))
    }

    #[cfg(windows)]
    #[allow(clippy::needless_pass_by_ref_mut)]
    fn for_child(child: &mut tokio::process::Child) -> Result<Self, String> {
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err("cannot create job object".into());
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err("cannot configure job object".into());
            }
            let Some(handle) = child.raw_handle() else {
                return Err("child has no process handle".into());
            };
            if AssignProcessToJobObject(job, handle as _) == 0 {
                return Err("cannot assign child to job object".into());
            }
            Ok(Self::Job(job))
        }
    }

    /// Terminate the tree: graceful (SIGTERM) by default, forced (SIGKILL /
    /// TerminateJobObject) with `force`. Idempotent.
    fn kill(&self, force: bool) {
        // the enum carries exactly one variant per platform, so the binding is direct
        #[cfg(unix)]
        let Self::Group(pid) = self;
        #[cfg(windows)]
        let Self::Job(job) = self;
        #[cfg(unix)]
        {
            let sig = if force { libc::SIGKILL } else { libc::SIGTERM };
            unsafe {
                libc::killpg(*pid as libc::pid_t, sig);
            }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::JobObjects::TerminateJobObject;
            // job-object termination is already tree-wide; there is no separate
            // graceful form to honor `force` with
            let _ = force;
            unsafe {
                TerminateJobObject(*job, 1);
            }
        }
    }
}

#[cfg(windows)]
impl Drop for ChildKill {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        let Self::Job(job) = *self;
        unsafe {
            CloseHandle(job);
        }
    }
}

/// The per-platform kill handle for one freshly spawned child.
fn child_kill(child: &mut tokio::process::Child) -> Result<ChildKill, String> {
    #[cfg(unix)]
    {
        ChildKill::for_child(child.id().unwrap_or(0))
    }
    #[cfg(windows)]
    {
        ChildKill::for_child(child)
    }
}

/// Kills the child's process group when dropped unless disarmed — the exec guarantee that
/// a run ending first (deadline, cancellation, dropped promise) never leaves the child alive.
struct KillGuard(Option<ChildKill>);

impl KillGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for KillGuard {
    fn drop(&mut self) {
        if let Some(kill) = self.0.take() {
            kill.kill(true);
        }
    }
}

// ---------------------------------------------------------------------------
// Exec capture: head-plus-tail with an omitted-byte count in the middle
// ---------------------------------------------------------------------------

struct HeadTail {
    head: Vec<u8>,
    tail: Vec<u8>,
    omitted: usize,
}

impl HeadTail {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: Vec::new(),
            omitted: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        let mut rest = data;
        if self.head.len() < CAPTURE_EDGE {
            let take = rest.len().min(CAPTURE_EDGE - self.head.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() {
            return;
        }
        let room = CAPTURE_EDGE.saturating_sub(self.tail.len());
        if rest.len() <= room {
            self.tail.extend_from_slice(rest);
        } else {
            let overflow = rest.len() - room;
            self.tail.extend_from_slice(&rest[overflow..]);
            self.omitted += overflow;
        }
    }

    fn render(&self) -> String {
        if self.omitted == 0 {
            let mut all = self.head.clone();
            all.extend_from_slice(&self.tail);
            String::from_utf8_lossy(&all).into_owned()
        } else {
            format!(
                "{}\n…[{} bytes omitted]…\n{}",
                String::from_utf8_lossy(&self.head),
                self.omitted,
                String::from_utf8_lossy(&self.tail),
            )
        }
    }
}

async fn capture_pipe<R: tokio::io::AsyncRead + Unpin>(
    mut pipe: Option<R>,
    capture: &mut HeadTail,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 16384];
    if let Some(reader) = pipe.as_mut() {
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => capture.push(&buf[..n]),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Spawn log: one append-only file, capped at 4 MiB, stable line numbers forever
// ---------------------------------------------------------------------------

struct LogWriter {
    file: std::fs::File,
    bytes: u64,
    capped: bool,
    last_was_newline: bool,
}

impl LogWriter {
    fn new(file: std::fs::File) -> Self {
        Self {
            file,
            bytes: 0,
            capped: false,
            last_was_newline: true,
        }
    }

    fn append(&mut self, data: &[u8]) -> u64 {
        if self.capped || data.is_empty() {
            return self.bytes;
        }
        let room = (LOG_CAP - self.bytes).min(data.len() as u64) as usize;
        let _ = self.file.write_all(&data[..room]);
        if room > 0 {
            self.last_was_newline = data[room - 1] == b'\n';
        }
        self.bytes += room as u64;
        if self.bytes >= LOG_CAP {
            let lead = if self.last_was_newline { "" } else { "\n" };
            let _ = writeln!(
                self.file,
                "{lead}…[process log reached the 4 MiB cap; further output was dropped]"
            );
            self.capped = true;
            self.last_was_newline = true;
        }
        self.bytes
    }
}

fn read_tail(path: &Path, cap: usize) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    let start = size.saturating_sub(cap as u64);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

// ---------------------------------------------------------------------------
// Process table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct ProcSnap {
    bytes: u64,
    running: bool,
}

struct EntryState {
    running: bool,
    code: Option<i32>,
    used: Instant,
}

/// One session-scoped process. Dead entries stay for post-mortem `status`/`kill`.
struct ProcEntry {
    id: String,
    exe: String,
    log_display: String,
    log_path: PathBuf,
    pid: u32,
    kill: Mutex<Option<ChildKill>>,
    state: Mutex<EntryState>,
    watch: watch::Sender<ProcSnap>,
}

impl ProcEntry {
    fn snapshot(&self) -> (u64, bool) {
        let snap = *self.watch.borrow();
        (snap.bytes, snap.running)
    }

    fn is_running(&self) -> bool {
        self.snapshot().1
    }

    fn terminate(&self, force: bool) {
        if let Some(kill) = self.kill.lock().unwrap().as_ref() {
            kill.kill(force);
        }
    }
}

/// The model-visible status record: identity, durable log path, liveness, exit code.
fn record_to_js<'js>(ctx: &Ctx<'js>, entry: &Arc<ProcEntry>) -> rquickjs::Result<Object<'js>> {
    let state = entry.state.lock().unwrap();
    let obj = Object::new(ctx.clone())?;
    obj.set("id", entry.id.clone())?;
    obj.set("log", entry.log_display.clone())?;
    obj.set("running", state.running)?;
    match state.code {
        Some(code) => obj.set("code", code)?,
        None => obj.set("code", Value::new_null(ctx.clone()))?,
    }
    Ok(obj)
}

struct TableInner {
    next: u64,
    entries: Vec<Arc<ProcEntry>>,
    exits: Arc<Mutex<Vec<serde_json::Value>>>,
}

/// The host-owned, in-memory, session-scoped process table. Not durable: after a restart
/// every old handle is `process_lost`, and the logs remain readable as ordinary files.
pub struct ProcTable {
    root: PathBuf,
    inner: Arc<Mutex<TableInner>>,
}

impl ProcTable {
    pub fn new(log_root: PathBuf) -> Self {
        Self {
            root: log_root,
            inner: Arc::new(Mutex::new(TableInner {
                next: 0,
                entries: Vec::new(),
                exits: Arc::new(Mutex::new(Vec::new())),
            })),
        }
    }

    /// One line of live handles and executables for the runtime-state block.
    pub fn live_summary(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let live: Vec<String> = inner
            .entries
            .iter()
            .filter(|entry| entry.is_running())
            .map(|entry| format!("{} {} (pid {})", entry.id, entry.exe, entry.pid))
            .collect();
        if live.is_empty() {
            return "none".into();
        }
        let joined = live.join(", ");
        if joined.chars().count() > 160 {
            format!("{}…", joined.chars().take(160).collect::<String>())
        } else {
            joined
        }
    }

    fn lookup(&self, id: &str) -> Result<Arc<ProcEntry>, String> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
            .inspect(|entry| {
                entry.state.lock().unwrap().used = Instant::now();
            })
            .ok_or_else(|| {
                format!(
                    "process_lost: handle {id:?} does not exist in this session; the process \
                     table is not durable and a handle from before a restart cannot be used, \
                     but the process log may still be readable as an ordinary file"
                )
            })
    }

    fn register(
        &self,
        exe: String,
        argv: &[String],
        cwd: &Path,
        pid: u32,
    ) -> Result<(Arc<ProcEntry>, serde_json::Value), String> {
        let mut inner = self.inner.lock().unwrap();
        let live = inner
            .entries
            .iter()
            .filter(|entry| entry.is_running())
            .count();
        if live >= MAX_LIVE {
            return Err(format!(
                "process_limit: the session process table holds {MAX_LIVE} live processes; \
                 kill one before spawning another"
            ));
        }
        if inner.entries.len() >= MAX_ENTRIES {
            // evict the least-recently-used dead entry; the host never kills a live
            // process to make room
            let mut victim = None;
            let mut oldest = Instant::now();
            for (index, entry) in inner.entries.iter().enumerate() {
                if !entry.is_running() {
                    let used = entry.state.lock().unwrap().used;
                    if used <= oldest {
                        oldest = used;
                        victim = Some(index);
                    }
                }
            }
            match victim {
                Some(index) => {
                    inner.entries.remove(index);
                }
                None => {
                    return Err(format!(
                        "process_limit: the session process table holds {MAX_ENTRIES} entries"
                    ))
                }
            }
        }
        inner.next += 1;
        let id = format!("p{}", inner.next);
        let log_path = self.root.join(format!("{id}.log"));
        let log_display = log_path.to_string_lossy().replace('\\', "/");
        let (watch_tx, _watch_rx) = watch::channel(ProcSnap {
            bytes: 0,
            running: true,
        });
        let entry = Arc::new(ProcEntry {
            id: id.clone(),
            exe: exe.clone(),
            log_display: log_display.clone(),
            log_path: log_path.clone(),
            pid,
            // armed by run_spawn once registration and the log file both succeeded
            kill: Mutex::new(None),
            state: Mutex::new(EntryState {
                running: true,
                code: None,
                used: Instant::now(),
            }),
            watch: watch_tx,
        });
        inner.entries.push(entry.clone());
        let receipt = json!({
            "exe": exe,
            "argv": argv,
            "cwd": cwd.to_string_lossy().replace('\\', "/"),
            "pid": pid,
            "handle": id,
            "log": log_display,
        });
        Ok((entry, receipt))
    }

    /// Drop a registration whose setup failed after it was inserted. The child is
    /// already dead (the guard killed it on the error path), so the entry must not
    /// linger as a phantom running slot.
    fn forget(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.retain(|entry| entry.id != id);
    }

    /// Terminate every live process (session end). Receipts for these kills land in the
    /// exit queue if a pump observes them in time.
    pub fn kill_all(&self, force: bool) {
        let entries: Vec<Arc<ProcEntry>> = {
            let inner = self.inner.lock().unwrap();
            inner.entries.clone()
        };
        for entry in entries {
            if entry.is_running() {
                entry.terminate(force);
            }
        }
    }

    /// Pending `proc/exit` receipts observed by pump tasks (processes that died between
    /// runs, or while awaiting a model).
    pub fn take_exit_receipts(&self) -> Vec<serde_json::Value> {
        let inner = self.inner.lock().unwrap();
        let mut queue = inner.exits.lock().unwrap();
        std::mem::take(&mut queue)
    }

    /// Give every pump a moment to observe forced exits, then drain the exit receipts.
    pub async fn shutdown(&self) -> Vec<serde_json::Value> {
        self.kill_all(true);
        let _ = tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        self.take_exit_receipts()
    }
}

/// The pump: drains both pipes into the log, waits for exit, records the final state and
/// the `proc/exit` receipt. One task per spawned process, owned by the host session.
async fn pump_process(
    entry: Arc<ProcEntry>,
    mut child: tokio::process::Child,
    file: std::fs::File,
    exits: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    let writer = Arc::new(Mutex::new(LogWriter::new(file)));
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_task = tokio::spawn(pump_pipe(stdout, writer.clone(), entry.watch.clone()));
    let err_task = tokio::spawn(pump_pipe(stderr, writer.clone(), entry.watch.clone()));
    let _ = out_task.await;
    let _ = err_task.await;
    let status = child.wait().await;
    let code = status.ok().and_then(|status| status.code());
    let bytes = writer.lock().unwrap().bytes;
    {
        let mut state = entry.state.lock().unwrap();
        state.running = false;
        state.code = code;
    }
    // the job object / group kill handle has no further use once the process is gone
    *entry.kill.lock().unwrap() = None;
    entry.watch.send_replace(ProcSnap {
        bytes,
        running: false,
    });
    let mut queue = exits.lock().unwrap();
    if queue.len() < EXIT_QUEUE_CAP {
        queue.push(json!({
            "handle": entry.id,
            "code": code,
            "tail": read_tail(&entry.log_path, EXIT_TAIL_CAP),
        }));
    }
}

async fn pump_pipe<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut pipe: Option<R>,
    writer: Arc<Mutex<LogWriter>>,
    watch_tx: watch::Sender<ProcSnap>,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 16384];
    let Some(reader) = pipe.as_mut() else {
        return;
    };
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let bytes = writer.lock().unwrap().append(&buf[..n]);
                watch_tx.send_replace(ProcSnap {
                    bytes,
                    running: true,
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Live output view: the same stream as the log file, numbered like host.fs.read
// ---------------------------------------------------------------------------

struct LogCursor {
    file: Option<std::fs::File>,
    offset: u64,
    next_no: u64,
    carry: Vec<u8>,
    done: bool,
    /// set by `flush_final`: if the terminated form of that line arrives later, its
    /// leading `\n` closes the already-emitted line and must not open an empty one
    awaiting_terminator: bool,
}

impl LogCursor {
    fn new() -> Self {
        Self {
            file: None,
            offset: 0,
            next_no: 1,
            carry: Vec::new(),
            done: false,
            awaiting_terminator: false,
        }
    }

    /// Read newly appended bytes and split complete lines. Line semantics match
    /// `host.fs.read`: split on `\n`, strip one trailing `\r`, count a final unterminated
    /// line, numbers start at 1.
    fn drain_lines(&mut self, path: &Path) -> Result<Vec<OutputLine>, String> {
        let mut fresh = Vec::new();
        if self.file.is_none() {
            self.file =
                Some(std::fs::File::open(path).map_err(|e| format!("cannot open log: {e}"))?);
        }
        let file = self.file.as_mut().expect("log handle");
        if file.seek(SeekFrom::Start(self.offset)).is_ok() {
            let _ = file.read_to_end(&mut fresh);
            self.offset += fresh.len() as u64;
        }
        self.carry.extend_from_slice(&fresh);
        if self.awaiting_terminator {
            self.awaiting_terminator = false;
            if self.carry.first() == Some(&b'\n') {
                self.carry.remove(0);
            }
        }
        let mut lines = Vec::new();
        while let Some(at) = self.carry.iter().position(|b| *b == b'\n') {
            let raw: Vec<u8> = self.carry.drain(..=at).collect();
            let mut text = String::from_utf8_lossy(&raw[..raw.len() - 1]).into_owned();
            if text.ends_with('\r') {
                text.pop();
            }
            lines.push(OutputLine {
                no: self.next_no,
                text,
            });
            self.next_no += 1;
        }
        Ok(lines)
    }

    /// The final unterminated line, if any, once the process has exited.
    fn flush_final(&mut self) -> Option<OutputLine> {
        if self.carry.is_empty() {
            return None;
        }
        let text = String::from_utf8_lossy(&self.carry).into_owned();
        self.carry.clear();
        self.awaiting_terminator = true;
        let no = self.next_no;
        self.next_no += 1;
        Some(OutputLine { no, text })
    }
}

struct OutputLine {
    no: u64,
    text: String,
}

impl<'js> rquickjs::IntoJs<'js> for OutputLine {
    fn into_js(self, ctx: &rquickjs::Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        let obj = Object::new(ctx.clone())?;
        obj.set("no", self.no as f64)?;
        obj.set("text", self.text)?;
        Ok(obj.into_value())
    }
}

async fn output_next(
    cursor: &RefCell<LogCursor>,
    entry: &Arc<ProcEntry>,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Vec<OutputLine>, String> {
    // subscribe before the first state check: any pump update after the check must wake us
    let mut changed = entry.watch.subscribe();
    loop {
        {
            let mut cursor = cursor.borrow_mut();
            if cursor.done {
                return Ok(Vec::new());
            }
            let lines = cursor.drain_lines(&entry.log_path)?;
            if !lines.is_empty() {
                return Ok(lines);
            }
            if !entry.is_running() {
                let final_line = cursor.flush_final();
                cursor.done = true;
                return Ok(final_line.into_iter().collect());
            }
        }
        tokio::select! {
            result = changed.changed() => {
                if result.is_ok() {
                    let _ = changed.borrow_and_update();
                }
            }
            _ = crate::kernel::cancelled(cancel) => return Ok(Vec::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// exec and spawn
// ---------------------------------------------------------------------------

struct ExecOutcome {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run_exec(
    authority: &ProcAuthority,
    exe: &str,
    argv: &[String],
    cwd: Option<&str>,
    working_root: &Path,
    cancel: &mut watch::Receiver<bool>,
    receipts: &Rc<RefCell<Vec<serde_json::Value>>>,
) -> Result<ExecOutcome, String> {
    let (exe_path, cwd_path) = authority.authorize(exe, argv, cwd, working_root)?;
    let mut cmd = tokio::process::Command::new(&exe_path);
    cmd.args(argv)
        .current_dir(&cwd_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    prepare_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {exe:?}: {e}"))?;
    let pid = child.id().unwrap_or(0);
    let kill = child_kill(&mut child).map_err(|error| {
        // a child we cannot arm a kill handle for must not outlive the call
        let _ = child.start_kill();
        format!("cannot arm the kill handle for {exe:?}: {error}")
    })?;
    let mut guard = KillGuard(Some(kill));
    {
        let mut queue = receipts.borrow_mut();
        queue.push(json!({
            "exe": exe_path.to_string_lossy().replace('\\', "/"),
            "argv": argv,
            "cwd": cwd_path.to_string_lossy().replace('\\', "/"),
            "pid": pid,
        }));
    }
    let mut stdout_cap = HeadTail::new();
    let mut stderr_cap = HeadTail::new();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let result = tokio::select! {
        status = async {
            let ((), ()) = tokio::join!(
                capture_pipe(stdout.as_mut(), &mut stdout_cap),
                capture_pipe(stderr.as_mut(), &mut stderr_cap),
            );
            child.wait().await
        } => status,
        _ = crate::kernel::cancelled(cancel) => {
            return Err(
                "the run ended before the command completed; the child's process group was \
                 killed"
                    .into(),
            );
        }
    };
    guard.disarm();
    let code = result.ok().and_then(|status| status.code());
    let stdout = stdout_cap.render();
    let stderr = stderr_cap.render();
    let tail = {
        let mut tail = String::new();
        if !stdout.is_empty() {
            tail.push_str(&crate::kernel::truncate_utf8(&stdout, 512));
        }
        if !stderr.is_empty() {
            if !tail.is_empty() {
                tail.push('\n');
            }
            tail.push_str(&crate::kernel::truncate_utf8(&stderr, 512));
        }
        crate::kernel::truncate_utf8(&tail, EXIT_TAIL_CAP)
    };
    {
        let mut queue = receipts.borrow_mut();
        queue.push(json!({"code": code, "tail": tail}));
    }
    Ok(ExecOutcome {
        code,
        stdout,
        stderr,
    })
}

struct SpawnOutcome {
    entry: Arc<ProcEntry>,
}

async fn run_spawn(
    authority: &ProcAuthority,
    table: &ProcTable,
    exe: &str,
    argv: &[String],
    cwd: Option<&str>,
    working_root: &Path,
    receipts: &Rc<RefCell<Vec<serde_json::Value>>>,
) -> Result<SpawnOutcome, String> {
    let (exe_path, cwd_path) = authority.authorize(exe, argv, cwd, working_root)?;
    let mut cmd = tokio::process::Command::new(&exe_path);
    cmd.args(argv)
        .current_dir(&cwd_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    prepare_command(&mut cmd);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot start {exe:?}: {e}"))?;
    let pid = child.id().unwrap_or(0);
    // the guard owns the kill handle until the entry takes it over: a registration
    // failure (table limits, log errors) must never leak a live child
    let kill = child_kill(&mut child).map_err(|error| {
        let _ = child.start_kill();
        format!("cannot arm the kill handle for {exe:?}: {error}")
    })?;
    let mut guard = KillGuard(Some(kill));
    std::fs::create_dir_all(&table.root)
        .map_err(|e| format!("cannot create log directory: {e}"))?;
    let (entry, receipt) = table.register(
        exe_path.to_string_lossy().replace('\\', "/"),
        argv,
        &cwd_path,
        pid,
    )?;
    let file = match std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&entry.log_path)
    {
        Ok(file) => file,
        Err(e) => {
            // the guard kills the child on return; the registration must not linger
            // as a phantom running entry holding one of the live slots
            table.forget(&entry.id);
            return Err(format!("cannot create process log: {e}"));
        }
    };
    {
        let kill = guard.0.take().expect("armed kill handle");
        *entry.kill.lock().unwrap() = Some(kill);
    }
    {
        let mut queue = receipts.borrow_mut();
        queue.push(receipt.clone());
    }
    let exits = {
        let inner = table.inner.lock().unwrap();
        inner.exits.clone()
    };
    tokio::spawn(pump_process(entry.clone(), child, file, exits));
    Ok(SpawnOutcome { entry })
}

async fn run_wait<'js>(
    ctx: &Ctx<'js>,
    table: &ProcTable,
    id: &str,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Object<'js>, String> {
    let entry = table.lookup(id)?;
    let mut watch_rx = entry.watch.subscribe();
    loop {
        if !entry.is_running() {
            return record_to_js(ctx, &entry).map_err(|e| e.to_string());
        }
        tokio::select! {
            result = watch_rx.changed() => {
                if result.is_ok() {
                    let _ = watch_rx.borrow_and_update();
                }
            }
            _ = crate::kernel::cancelled(cancel) => {
                return Err(format!(
                    "the run deadline expired while waiting for process {id}; the process \
                     keeps running"
                ));
            }
        }
    }
}

async fn run_kill<'js>(
    ctx: &Ctx<'js>,
    table: &ProcTable,
    id: &str,
    force: bool,
    cancel: &mut watch::Receiver<bool>,
) -> Result<Object<'js>, String> {
    let entry = table.lookup(id)?;
    entry.terminate(force);
    run_wait(ctx, table, id, cancel).await
}

// ---------------------------------------------------------------------------
// Capability installation
// ---------------------------------------------------------------------------

fn js_err(msg: String) -> rquickjs::Error {
    rquickjs::Error::FromJs {
        from: "proc",
        to: "result",
        message: Some(msg),
    }
}

fn parse_argv(value: Value<'_>) -> Result<Vec<String>, String> {
    let array = value.as_array().ok_or_else(|| {
        "argv must be an array of strings, e.g. [\"test\", \"--\", \"--nocapture\"]".to_string()
    })?;
    let mut argv = Vec::with_capacity(array.len());
    for item in array.iter() {
        let value: Value = item.map_err(|_| "argv must contain only strings".to_string())?;
        let arg = value
            .as_string()
            .ok_or_else(|| "argv must contain only strings".to_string())?
            .to_string()
            .map_err(|_| "argv must contain only valid UTF-8 strings".to_string())?;
        argv.push(arg);
    }
    Ok(argv)
}

fn parse_cwd(opts: &Opt<Object<'_>>) -> Result<Option<String>, String> {
    match opts.0.as_ref() {
        None => Ok(None),
        Some(object) => match object.get::<_, Option<String>>("cwd") {
            Ok(Some(cwd)) => Ok(Some(cwd)),
            Ok(None) => Ok(None),
            Err(_) => Err("options.cwd must be a string absolute path".into()),
        },
    }
}

fn parse_force(opts: &Opt<Object<'_>>) -> Result<bool, String> {
    match opts.0.as_ref() {
        None => Ok(false),
        Some(object) => match object.get::<_, Option<bool>>("force") {
            Ok(Some(force)) => Ok(force),
            Ok(None) => Ok(false),
            Err(_) => Err("options.force must be a boolean".into()),
        },
    }
}

fn build_exec_result<'js>(
    ctx: &Ctx<'js>,
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    match code {
        Some(code) => obj.set("code", code)?,
        None => obj.set("code", Value::new_null(ctx.clone()))?,
    }
    obj.set("stdout", stdout)?;
    obj.set("stderr", stderr)?;
    Ok(obj)
}

fn build_spawn_result<'js>(
    ctx: &Ctx<'js>,
    entry: &Arc<ProcEntry>,
    cancel: &watch::Receiver<bool>,
) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("id", entry.id.clone())?;
    obj.set("log", entry.log_display.clone())?;
    let cursor: Rc<RefCell<LogCursor>> = Rc::new(RefCell::new(LogCursor::new()));
    let iter_entry = entry.clone();
    let iter_cancel = cancel.clone();
    let next_fn = Function::new(
        ctx.clone(),
        Async(move || {
            let cursor = cursor.clone();
            let entry = iter_entry.clone();
            let mut cancel = iter_cancel.clone();
            async move {
                output_next(&cursor, &entry, &mut cancel)
                    .await
                    .map_err(js_err)
            }
        }),
    )?;
    let iter = Object::new(ctx.clone())?;
    iter.set("next", next_fn)?;
    obj.set("output", iter)?;
    Ok(obj)
}

/// Registers the host.proc namespace. Both verbs check the frozen command authority at
/// every call; receipts flow into the per-run receipt queue for the journal.
pub(crate) fn install<'js>(
    ctx: &Ctx<'js>,
    host: &Object<'js>,
    authority: &ProcAuthority,
    table: &Rc<ProcTable>,
    working_root: &Path,
    receipts: &Rc<RefCell<Vec<serde_json::Value>>>,
    cancel: &watch::Receiver<bool>,
) -> rquickjs::Result<()> {
    let obj = Object::new(ctx.clone())?;

    let exec_auth = authority.clone();
    let exec_root = working_root.to_path_buf();
    let exec_receipts = receipts.clone();
    let exec_cancel = cancel.clone();
    let exec_fn = Function::new(
        ctx.clone(),
        Async(
            move |ctx: Ctx<'js>, exe: String, argv: Value<'js>, opts: Opt<Object<'js>>| {
                let authority = exec_auth.clone();
                let working_root = exec_root.clone();
                let receipts = exec_receipts.clone();
                let mut cancel = exec_cancel.clone();
                async move {
                    let argv = parse_argv(argv).map_err(js_err)?;
                    let cwd = parse_cwd(&opts).map_err(js_err)?;
                    let outcome = run_exec(
                        &authority,
                        &exe,
                        &argv,
                        cwd.as_deref(),
                        &working_root,
                        &mut cancel,
                        &receipts,
                    )
                    .await
                    .map_err(js_err)?;
                    build_exec_result(&ctx, outcome.code, &outcome.stdout, &outcome.stderr)
                }
            },
        ),
    )?;
    obj.set("exec", exec_fn)?;

    let spawn_auth = authority.clone();
    let spawn_table = table.clone();
    let spawn_root = working_root.to_path_buf();
    let spawn_receipts = receipts.clone();
    let spawn_cancel = cancel.clone();
    let spawn_fn = Function::new(
        ctx.clone(),
        Async(
            move |ctx: Ctx<'js>, exe: String, argv: Value<'js>, opts: Opt<Object<'js>>| {
                let authority = spawn_auth.clone();
                let table = spawn_table.clone();
                let working_root = spawn_root.clone();
                let receipts = spawn_receipts.clone();
                let cancel = spawn_cancel.clone();
                async move {
                    let argv = parse_argv(argv).map_err(js_err)?;
                    let cwd = parse_cwd(&opts).map_err(js_err)?;
                    let outcome = run_spawn(
                        &authority,
                        &table,
                        &exe,
                        &argv,
                        cwd.as_deref(),
                        &working_root,
                        &receipts,
                    )
                    .await
                    .map_err(js_err)?;
                    build_spawn_result(&ctx, &outcome.entry, &cancel)
                }
            },
        ),
    )?;
    obj.set("spawn", spawn_fn)?;

    let status_table = table.clone();
    let status_fn = Function::new(ctx.clone(), move |ctx: Ctx<'js>, id: String| {
        status_table
            .lookup(&id)
            .and_then(|entry| record_to_js(&ctx, &entry).map_err(|e| e.to_string()))
            .map_err(js_err)
    })?;
    obj.set("status", status_fn)?;

    let wait_table = table.clone();
    let wait_cancel = cancel.clone();
    let wait_fn = Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, id: String| {
            let table = wait_table.clone();
            let mut cancel = wait_cancel.clone();
            async move {
                run_wait(&ctx, &table, &id, &mut cancel)
                    .await
                    .map_err(js_err)
            }
        }),
    )?;
    obj.set("wait", wait_fn)?;

    let kill_table = table.clone();
    let kill_cancel = cancel.clone();
    let kill_fn = Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, id: String, opts: Opt<Object<'js>>| {
            let table = kill_table.clone();
            let mut cancel = kill_cancel.clone();
            async move {
                let force = parse_force(&opts).map_err(js_err)?;
                run_kill(&ctx, &table, &id, force, &mut cancel)
                    .await
                    .map_err(js_err)
            }
        }),
    )?;
    obj.set("kill", kill_fn)?;

    host.set("proc", obj)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("terrarium-proc-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// The display spelling a model would use for a resolved executable: strips the
    /// verbatim `\\?\` prefix Windows canonicalize() adds and normalizes separators.
    fn exe_alias(exe: &std::path::Path) -> String {
        let text = exe.to_string_lossy();
        text.strip_prefix(r"\\?\")
            .unwrap_or(&text)
            .replace('\\', "/")
    }

    #[test]
    fn head_tail_keeps_head_and_tail_with_omitted_count() {
        let mut cap = HeadTail::new();
        cap.push(b"head");
        assert_eq!(cap.render(), "head");
        cap.push(&vec![b'x'; CAPTURE_STREAM * 2]);
        let rendered = cap.render();
        assert!(rendered.contains("head"), "{rendered}");
        assert!(rendered.contains("bytes omitted"), "{rendered}");
        assert!(rendered.ends_with(&"x".repeat(CAPTURE_EDGE)), "{rendered}");
        let omitted = CAPTURE_STREAM * 2 - (CAPTURE_EDGE - "head".len()) - CAPTURE_EDGE;
        assert!(rendered.contains(&omitted.to_string()), "{rendered}");
    }

    #[test]
    fn log_writer_caps_and_marks() {
        let root = tmp_root("log-cap");
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(root.join("p1.log"))
            .unwrap();
        let mut writer = LogWriter::new(file);
        writer.append(b"first\n");
        writer.append(&vec![b'a'; LOG_CAP as usize]);
        let bytes_after_cap = writer.bytes;
        writer.append(b"late\n");
        assert_eq!(writer.bytes, bytes_after_cap);
        let text = std::fs::read_to_string(root.join("p1.log")).unwrap();
        assert!(text.starts_with("first\n"), "{text}");
        assert!(text.contains("4 MiB cap"), "{text}");
        assert!(!text.contains("late"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn log_cursor_counts_lines_like_fs_read() {
        let root = tmp_root("cursor");
        let path = root.join("p1.log");
        std::fs::write(&path, "one\r\ntwo\nthree").unwrap();
        let mut cursor = LogCursor::new();
        let lines = cursor.drain_lines(&path).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "one");
        assert_eq!(lines[0].no, 1);
        assert_eq!(lines[1].text, "two");
        assert_eq!(lines[1].no, 2);
        assert!(cursor.drain_lines(&path).unwrap().is_empty());
        assert_eq!(cursor.flush_final().unwrap().text, "three");
        // appended bytes are picked up from the same cursor
        std::fs::write(&path, "one\r\ntwo\nthree\nfour\n").unwrap();
        let lines = cursor.drain_lines(&path).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "four");
        assert_eq!(lines[0].no, 4);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn resolve_executable_rejects_bad_spellings() {
        assert!(resolve_executable("").is_err());
        assert!(resolve_executable("./local/tool").is_err());
        assert!(resolve_executable("definitely-not-a-real-tool-xyz").is_err());
        // a separator-bearing spelling must be absolute and must exist
        let missing = if cfg!(windows) {
            "C:/definitely/not/here.exe"
        } else {
            "/definitely/not/here"
        };
        assert!(resolve_executable(missing).is_err());
        // an existing absolute executable resolves to its canonical path
        #[cfg(unix)]
        {
            let resolved = resolve_executable("/bin/sh").unwrap();
            assert_eq!(resolved, std::fs::canonicalize("/bin/sh").unwrap());
        }
    }

    #[test]
    fn resolve_cwd_defaults_to_working_root() {
        let raw_root = tmp_root("cwd");
        let dir = raw_root.join("sub");
        std::fs::create_dir(&dir).unwrap();
        // production always passes an already-canonical working root; macOS temp
        // dirs (/var -> /private/var) would otherwise differ from canonicalize().
        // The user-supplied cwd stays in display form: verbatim \\?\ paths are a
        // canonicalize() output on Windows, not a valid model-side spelling.
        let root = raw_root.canonicalize().unwrap();
        assert_eq!(resolve_cwd(None, &root).unwrap(), root);
        assert_eq!(
            resolve_cwd(Some(&dir.to_string_lossy()), &root).unwrap(),
            dir.canonicalize().unwrap()
        );
        assert!(resolve_cwd(Some(&dir.join("missing").to_string_lossy()), &root).is_err());
        assert!(resolve_cwd(Some("relative"), &root).is_err());
        let file = root.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(resolve_cwd(Some(&file.to_string_lossy()), &root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn authority_matching_is_identity_argv_and_cwd() {
        let root = tmp_root("authority").canonicalize().unwrap();
        #[cfg(unix)]
        let (exe, switch) = (resolve_executable("/bin/sh").unwrap(), "-c");
        #[cfg(windows)]
        let (exe, switch) = (
            resolve_executable("C:/Windows/System32/cmd.exe").unwrap(),
            "/c",
        );
        let records = vec![CommandRecord {
            exe: exe.clone(),
            argv: vec![switch.into(), "echo hi".into()],
            cwd: root.clone(),
        }];
        let authority = ProcAuthority::Allowed(CommandSet {
            grants: Vec::new(),
            records,
        });
        let (ok_exe, ok_cwd) = authority
            .authorize(
                &exe_alias(&exe),
                &[switch.into(), "echo hi".into()],
                None,
                &root,
            )
            .unwrap();
        assert_eq!(ok_exe, exe);
        assert_eq!(ok_cwd, root);

        // argv mismatch, cwd mismatch, and undeclared executables all fail with the
        // corrective error carrying the expected record
        let wrong = authority
            .authorize(
                &exe_alias(&exe),
                &[switch.into(), "echo bye".into()],
                None,
                &root,
            )
            .unwrap_err();
        assert!(wrong.contains("command_not_authorized"), "{wrong}");
        assert!(wrong.contains("expected"), "{wrong}");
        assert!(wrong.contains("\"argv\""), "{wrong}");

        assert!(ProcAuthority::Denied
            .authorize(&exe_alias(&exe), &[], None, &root)
            .is_err());
        assert!(ProcAuthority::Unrestricted
            .authorize(
                &exe_alias(&exe),
                &[switch.into(), "anything".into()],
                None,
                &root
            )
            .is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    // ------------------------------------------------------------------
    // End-to-end through the cage: the same road a model program takes
    // ------------------------------------------------------------------

    fn proc_env(root: &Path, authority: ProcAuthority) -> crate::RunEnv {
        crate::RunEnv {
            fs: crate::fs::RunFilesystemAuthority::ReadOnly,
            proc: authority,
            net_offline: false,
            table: Rc::new(ProcTable::new(root.join("procs"))),
            working_root: root.canonicalize().unwrap(),
            receipts: crate::RunEnv::receipts(),
        }
    }

    fn allowed_for(exe_name: &str, argv: &[String], cwd: &Path) -> ProcAuthority {
        ProcAuthority::Allowed(CommandSet {
            grants: Vec::new(),
            records: vec![CommandRecord {
                exe: resolve_executable(exe_name).unwrap(),
                argv: argv.to_vec(),
                cwd: cwd.to_path_buf(),
            }],
        })
    }

    fn echo_command() -> (String, Vec<String>) {
        if cfg!(windows) {
            (
                "C:/Windows/System32/cmd.exe".into(),
                vec!["/c".into(), "echo hi".into()],
            )
        } else {
            ("sh".into(), vec!["-c".into(), "echo hi".into()])
        }
    }

    async fn eval(env: &crate::RunEnv, source: &str) -> crate::kernel::Outcome {
        let (tx, _rx) = watch::channel(false);
        crate::kernel::eval_js(source, 10_000, env, tx).await
    }

    #[test]
    fn exec_runs_the_approved_command_and_journals_receipts() {
        // forking tests serialize against session-journal tests: a child between fork
        // and exec briefly holds every open description, including locked journals
        let _journal_guard = crate::session::tests::STATE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let root = tmp_root("exec-e2e");
                let (exe, argv) = echo_command();
                let env = proc_env(
                    &root,
                    allowed_for(&exe, &argv, &root.canonicalize().unwrap()),
                );
                let source = format!(
                    "const r = await host.proc.exec('{}', {});\n\
             return {{code: r.code, out: r.stdout.trim()}}",
                    exe,
                    serde_json::to_string(&argv).unwrap()
                );
                let out = eval(&env, &source).await;
                assert!(out.ok, "error: {:?}", out.error);
                assert_eq!(out.value, Some(serde_json::json!({"code": 0, "out": "hi"})));
                // journal receipts: the launch, then the exit with a bounded tail
                assert_eq!(out.receipts.len(), 2);
                assert_eq!(out.receipts[0]["argv"], serde_json::json!(argv));
                assert!(out.receipts[0]["pid"].as_u64().unwrap() > 0);
                assert_eq!(out.receipts[1]["code"], serde_json::json!(0));
                assert!(out.receipts[1]["tail"].as_str().unwrap().contains("hi"));
                let _ = std::fs::remove_dir_all(&root);
            })
    }

    #[test]
    fn exec_outside_the_frozen_set_fails_with_the_corrective_error() {
        // forking tests serialize against session-journal tests: a child between fork
        // and exec briefly holds every open description, including locked journals
        let _journal_guard = crate::session::tests::STATE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let root = tmp_root("exec-deny");
                let env = proc_env(&root, ProcAuthority::Denied);
                let (exe, argv) = echo_command();
                let source = format!(
                    "return await host.proc.exec('{}', {})",
                    exe,
                    serde_json::to_string(&argv).unwrap()
                );
                let out = eval(&env, &source).await;
                assert!(!out.ok, "value: {:?}", out.value);
                let message = out.error.expect("denied exec").message;
                assert!(message.contains("command_not_authorized"), "{message}");
                assert!(message.contains("expected"), "{message}");
                assert!(out.receipts.is_empty());
                let _ = std::fs::remove_dir_all(&root);
            })
    }

    #[test]
    fn spawn_streams_output_then_waits_on_the_same_handle() {
        // forking tests serialize against session-journal tests: a child between fork
        // and exec briefly holds every open description, including locked journals
        let _journal_guard = crate::session::tests::STATE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let root = tmp_root("spawn-e2e");
                let (exe, script) = if cfg!(windows) {
                    (
                        "C:/Windows/System32/cmd.exe".to_string(),
                        "echo one & ping -n 2 127.0.0.1 >nul & echo two".to_string(),
                    )
                } else {
                    (
                        "sh".to_string(),
                        "echo one; sleep 0.2; echo two".to_string(),
                    )
                };
                let argv: Vec<String> = if cfg!(windows) {
                    vec!["/c".into(), script]
                } else {
                    vec!["-c".into(), script]
                };
                let env = proc_env(
                    &root,
                    allowed_for(&exe, &argv, &root.canonicalize().unwrap()),
                );
                let source = format!(
                    "const p = await host.proc.spawn('{}', {});\n\
             const lines = [];\n\
             for await (const line of p.output) lines.push(line.no + ':' + line.text.trim());\n\
             const w = await host.proc.wait(p.id);\n\
             const s = host.proc.status(p.id);\n\
             return {{id: p.id, log: p.log, lines, waited: w.running, status: s.running, \
                 code: w.code}}",
                    exe,
                    serde_json::to_string(&argv).unwrap()
                );
                let out = eval(&env, &source).await;
                assert!(out.ok, "error: {:?}", out.error);
                let value = out.value.expect("spawn result");
                assert_eq!(value["id"], serde_json::json!("p1"));
                assert_eq!(value["lines"], serde_json::json!(["1:one", "2:two"]));
                assert_eq!(value["waited"], serde_json::json!(false));
                assert_eq!(value["status"], serde_json::json!(false));
                assert_eq!(value["code"], serde_json::json!(0));
                // the durable log is an ordinary file the host can still read
                let log = std::path::PathBuf::from(value["log"].as_str().unwrap());
                let text = std::fs::read_to_string(&log).unwrap();
                assert!(text.contains("one"), "{text}");
                assert!(text.contains("two"), "{text}");
                // the spawn receipt carries the handle and the log path
                let receipt = &out.receipts[0];
                assert_eq!(receipt["handle"], serde_json::json!("p1"));
                assert!(receipt["log"].as_str().unwrap().ends_with("p1.log"));
                env.table.shutdown().await;
                let _ = std::fs::remove_dir_all(&root);
            })
    }

    #[test]
    fn kill_ends_a_spawned_process_and_stale_handles_report_process_lost() {
        // forking tests serialize against session-journal tests: a child between fork
        // and exec briefly holds every open description, including locked journals
        let _journal_guard = crate::session::tests::STATE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let root = tmp_root("spawn-kill");
                let (exe, argv): (String, Vec<String>) = if cfg!(windows) {
                    (
                        "C:/Windows/System32/cmd.exe".into(),
                        vec!["/c".into(), "ping -n 30 127.0.0.1 >nul".into()],
                    )
                } else {
                    ("sh".into(), vec!["-c".into(), "sleep 30".into()])
                };
                let env = proc_env(
                    &root,
                    allowed_for(&exe, &argv, &root.canonicalize().unwrap()),
                );
                let source = format!(
                    "const p = await host.proc.spawn('{}', {});\n\
             const k = await host.proc.kill(p.id, {{force: true}});\n\
             let lost = '';\n\
             try {{ host.proc.status('p404'); }} catch (e) {{ lost = String(e.message); }}\n\
             return {{running: k.running, lost}}",
                    exe,
                    serde_json::to_string(&argv).unwrap()
                );
                let out = eval(&env, &source).await;
                assert!(out.ok, "error: {:?}", out.error);
                let value = out.value.expect("kill result");
                assert_eq!(value["running"], serde_json::json!(false));
                assert!(value["lost"].as_str().unwrap().contains("process_lost"));
                // shut the table down fully: no child, pump task, or log handle survives the test
                env.table.shutdown().await;
                let _ = std::fs::remove_dir_all(&root);
            })
    }

    #[test]
    fn spawn_respects_the_live_process_limit() {
        // forking tests serialize against session-journal tests: a child between fork
        // and exec briefly holds every open description, including locked journals
        let _journal_guard = crate::session::tests::STATE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let root = tmp_root("spawn-limit");
                let (exe, argv): (String, Vec<String>) = if cfg!(windows) {
                    (
                        "C:/Windows/System32/cmd.exe".into(),
                        vec!["/c".into(), "ping -n 6 127.0.0.1 >nul".into()],
                    )
                } else {
                    ("sh".into(), vec!["-c".into(), "sleep 5".into()])
                };
                let env = proc_env(
                    &root,
                    allowed_for(&exe, &argv, &root.canonicalize().unwrap()),
                );
                let source = format!(
                    "const ids = [];\n\
             let err = '';\n\
             for (let i = 0; i < 9; i++) {{\n\
               try {{ const p = await host.proc.spawn('{}', {}); ids.push(p.id); }}\n\
               catch (e) {{ err = String(e.message); break; }}\n\
             }}\n\
             return {{count: ids.length, err}}",
                    exe,
                    serde_json::to_string(&argv).unwrap()
                );
                let out = eval(&env, &source).await;
                assert!(out.ok, "error: {:?}", out.error);
                let value = out.value.expect("limit result");
                assert_eq!(value["count"], serde_json::json!(MAX_LIVE));
                assert!(value["err"].as_str().unwrap().contains("process_limit"));
                env.table.shutdown().await;
                let _ = std::fs::remove_dir_all(&root);
            })
    }

    #[test]
    fn a_spawn_that_cannot_open_its_log_frees_its_table_entry() {
        // forking tests serialize against session-journal tests: a child between fork
        // and exec briefly holds every open description, including locked journals
        let _journal_guard = crate::session::tests::STATE_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let root = tmp_root("spawn-logfail");
                let procs_root = root.join("procs");
                std::fs::create_dir_all(&procs_root).unwrap();
                // the first registration picks p1: make its log path impossible to
                // open so the post-registration step fails
                std::fs::create_dir_all(procs_root.join("p1.log")).unwrap();
                let (exe, argv): (String, Vec<String>) = if cfg!(windows) {
                    (
                        "C:/Windows/System32/cmd.exe".into(),
                        vec!["/c".into(), "echo hi".into()],
                    )
                } else {
                    ("sh".into(), vec!["-c".into(), "echo hi".into()])
                };
                let env = proc_env(
                    &root,
                    allowed_for(&exe, &argv, &root.canonicalize().unwrap()),
                );
                let fail = format!(
                    "try {{ await host.proc.spawn('{}', {}); return 'no error'; }}\n\
             catch (e) {{ return String(e.message); }}",
                    exe,
                    serde_json::to_string(&argv).unwrap()
                );
                let out = eval(&env, &fail).await;
                assert!(out.ok, "error: {:?}", out.error);
                let message = out.value.expect("failure message");
                assert!(
                    message
                        .as_str()
                        .unwrap()
                        .contains("cannot create process log"),
                    "{message}"
                );
                // the failed entry must not linger as a phantom running slot: the
                // table still accepts another spawn (it becomes p2, not process_limit)
                let retry = format!(
                    "const p = await host.proc.spawn('{}', {});\nreturn p.id",
                    exe,
                    serde_json::to_string(&argv).unwrap()
                );
                let out = eval(&env, &retry).await;
                assert!(out.ok, "error: {:?}", out.error);
                assert_eq!(out.value, Some(serde_json::json!("p2")));
                env.table.shutdown().await;
                let _ = std::fs::remove_dir_all(&root);
            })
    }
}
