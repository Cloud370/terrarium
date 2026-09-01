//! Host API registry: the single source of truth for the model-visible API surface.
//! The contract renders this list into {{HOST_API}}; keep entries concise and behavioral.

pub struct HostDoc {
    pub sig: &'static str,
    pub doc: &'static str,
}

pub const HOST_API: &[HostDoc] = &[
    HostDoc {
        sig: "host.fs.list(dir)",
        doc: "→ sorted [{name, type, size}, ...] for one directory level. type is file, directory, symlink, or other; size is bytes for regular files and null otherwise",
    },
    HostDoc {
        sig: "host.fs.read(path, from, to)",
        doc: "→ 1-based inclusive lines as `N: text`; to=Infinity reads to EOF. A continuation footer appears only when more lines follow. Lines over 2000 characters are cut and total output over 64 MiB is rejected",
    },
    HostDoc {
        sig: "host.fs.text(path)",
        doc: "→ whole text file as an LF-normalized string with a leading BOM removed. Files over 64 MiB are rejected",
    },
    HostDoc {
        sig: "host.fs.replace(path, oldText, newText[, {all}])",
        doc: "→ {path, replacements, bytes}. Exact literal replacement; default requires one match, while {all: true} explicitly replaces every match. Empty, missing, ambiguous, and no-op edits fail. Writes through host.fs.write, so the target must be authorized for the run",
    },
    HostDoc {
        sig: "host.fs.scan(path, {glob?, contains?, skipDirs?, skipExts?, gitignore?, hidden?})",
        doc: "→ async {file, no, text} lines under a directory; for one known file use host.fs.read or host.fs.text. Optional contains is a Rust-side literal prefilter; JavaScript remains the final predicate. Defaults: respect .gitignore, skip hidden entries and binaries, never follow symlinks. glob filters files before opening them. A yield is one whole line; lines over 8 MiB are cut",
    },
    HostDoc {
        sig: "host.fs.walk(path, {glob?, skipDirs?, skipExts?, gitignore?, hidden?})",
        doc: "→ async {file, size} regular-file entries under a directory using scan's traversal options without opening contents. For one known file, use host.fs.read or host.fs.text. contains is rejected because it is scan-only",
    },
    HostDoc {
        sig: "host.fs.write(path, content)",
        doc: "→ bytes written. Text-only and atomic; creates approved missing parent directories. The exact target must be authorized for the current run (read-only denies every write) and preserves an existing file's BOM and line endings. The host separately records bounded write receipts",
    },
    HostDoc {
        sig: "host.proc.exec(exe, argv[, {cwd}])",
        doc: "→ await {code, stdout, stderr}. Runs one command to completion within the current run; argv is an array of strings with no shell. stdout and stderr are separate, each bounded to 16 KiB as head-plus-tail with an omitted-byte count. The exe and cwd must match a command declared in the ```access block (resolved executable, exact argv, cwd defaulting to the working root); a mismatch fails with command_not_authorized printing the expected records. If the run ends first the child's process group is killed",
    },
    HostDoc {
        sig: "host.proc.spawn(exe, argv[, {cwd}])",
        doc: "→ await {id, log, output}: a session-scoped process that outlives the run (dev servers, watchers). Authorization matches exec. id is an opaque handle valid for the whole session; log is a host-owned append-only file (4 MiB cap) readable with host.fs.read; output is a live async iterable of {no, text} lines, in this run only. Later runs read log and query host.proc.status(id)",
    },
    HostDoc {
        sig: "host.proc.status(id)",
        doc: "→ {id, log, running, code} from the session's in-memory table. An unknown or pre-restart handle is the error process_lost; the table holds at most 8 live processes and 16 entries",
    },
    HostDoc {
        sig: "host.proc.wait(id)",
        doc: "→ await of the final {id, log, running: false, code}; bounded by the run deadline (the process keeps running when the run dies)",
    },
    HostDoc {
        sig: "host.proc.kill(id[, {force}])",
        doc: "→ await of the final record after terminating the process group — graceful by default, forced with {force: true}. Idempotent on an exited process",
    },
    HostDoc {
        sig: "host.net.fetch(url[, {method, headers, body}])",
        doc: "→ await {status, finalUrl, body}. An http/https GET, HEAD, POST, PUT, PATCH, or DELETE run as the operating-system user — no consent, journaled per request. Header values are strings or {env: NAME} resolved host-side; credentials never enter the cage. body is a bounded string (streaming upload is a declared curl). body of the result is an async iterable of string chunks (lossy UTF-8); redirects are followed (at most 5), the final URL is reported. Limits: 60 s per request, 8 MiB response cap, 4 concurrent requests; --offline disables fetch for the whole invocation",
    },
];

/// API list for the contract (one per line: signature — doc)
pub fn api_lines() -> String {
    HOST_API
        .iter()
        .map(|d| format!("- {} — {}\n", d.sig, d.doc))
        .collect()
}

/// Sorted, de-duplicated capability namespaces ("host.fs"), rendered into the runtime state.
pub fn capability_namespaces() -> String {
    let mut namespaces: Vec<String> = HOST_API
        .iter()
        .filter_map(|doc| {
            let signature = doc.sig.split('(').next()?.trim();
            let mut parts: Vec<&str> = signature.split('.').collect();
            parts.pop()?; // drop the method name
            Some(parts.join("."))
        })
        .collect();
    namespaces.sort();
    namespaces.dedup();
    namespaces.join(", ")
}
