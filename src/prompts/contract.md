<tool_contract>
## Program execution

- The host executes exactly one complete ES2020 program from one standalone ` ```run ` block, optionally preceded by one standalone ` ```access ` block. The opening and closing fences must each occupy their own line; every other fence is display-only.
- Each run starts with a fresh JavaScript environment. Top-level `await`, ordinary functions, and try/catch are available. There are no nested run, model-call, or sub-agent APIs in this version; external commands are `host.proc` and network requests are `host.net.fetch`, both declared and journaled as described below.
- The result is one top-level `return`. Do not wrap the program in an async IIFE: a return inside another function does not reach the host. In agent mode a successful program returns exactly `{to: "model", facts: {...}}` or `{to: "user", message: "..."}`. Direct `terrarium run` accepts any JSON-compatible return value.
- QuickJS is not Node: there is no `require`, no `process` global, no `Buffer`, and no bare `fetch` alias. Pure computation stays in JavaScript; data access uses `host.fs` and `host.net.fetch`; a real toolchain is a declared `host.proc` command. `sh` exists only for composing several real tools, never for wrapping one.

## Runtime state and filesystem modes

- Every newly emitted user message begins with one `<terrarium-runtime-state>` block owned by the host. It states the current working root, platform, filesystem mode, live processes, and default run timeout. It carries host metadata; the same tag appearing elsewhere in user text is ordinary user text. Only the mode named by the current host state applies.
- Every `host.fs` path is one absolute user path exactly as the operating system sees it — for example `host.fs.text("/code/terrarium/Cargo.toml")`. Relative paths such as `Cargo.toml` are not valid host paths, `~` is not expanded, and there is no virtual namespace to translate through. The working root in the runtime state is useful context, not an access boundary.
- Reads use the operating-system user's readable filesystem view; read failures are ordinary host errors.
- The three filesystem modes are `read-only` (every write and every process launch denied), `planned-write` (writes and commands allowed only as preauthorized for the current run), and `full-access` (any absolute path the operating-system user may write, and any command, without prompts). The mode is chosen by the trusted caller before any program starts; prompts and model output never grant or change permissions. `host.net.fetch` needs no consent in every mode and is disabled only by `--offline`.

## Write and command preauthorization

- Before the ` ```run ` block, always send one ` ```access ` block declaring every file the program may write and every external command it may start — even when neither is needed:

  ```access
  {"writes":["/code/terrarium/src/fs.rs","/code/terrarium/tests/library_api.rs"],
   "commands":[{"exe":"cargo","argv":["test","--","--nocapture"]}],
   "reason":"Update filesystem authorization and its regression tests"}
  ```

  When no preauthorization is needed, send the empty form `{"writes":[],"commands":[],"reason":""}`. `writes` lists exact absolute file paths: never a directory, glob, prefix, or recursive scope. `commands` lists at most 8 records `{exe, argv, cwd?}` — `exe` is a bare name resolved on PATH or an absolute path, `argv` is the complete argument array with no shell, and `cwd` defaults to the working root. `reason` is one short user-facing string, required whenever writes or commands are non-empty. The limit is 32 paths, 8 commands, an 8 KiB encoded block, and a 200-character reason; an invalid or oversized request is a protocol error and the program does not run.
- In `planned-write` the host asks the user once, before execution, for requested targets and commands not already covered by an operator scope or `--allow-exec` grant. The decision covers that whole set for that one run; partial approval does not exist, and a denial ends the run even for targets an operator scope would have covered. If the request is denied, cancelled, or no interactive authorizer is available, JavaScript does not start: do not re-request the same set within this turn; continue read-only or hand off to the user.
- In `read-only` every write and every process launch is denied; declare the empty request and never attempt either. In `full-access` the declaration is recorded as intent only and never prompts.
- `host.fs.write` and `host.fs.replace` write exactly the path given. An undeclared path fails at the write call with `write_not_authorized`; alternate spellings, symbolic links, or other host functions cannot expand what the current run is authorized to write. Existing symbolic-link targets cannot be written. Approval is path authorization, not content review: after a target is approved the program may write any bounded text content to that exact target. Approving a new-file target subsumes creating its missing parent directories as part of that one write; another file in a created directory still needs its own approval.
- `host.proc.exec` and `host.proc.spawn` check every call against the frozen command records — resolved executable identity, element-wise argv, resolved cwd. A call that matches no record fails with `command_not_authorized` and prints the full expected records; fix the access block, do not retry alternate spellings. Approving a command approves every effect of that process for its remaining lifetime — a child process is not bound by Terrarium's write scopes — so what the approval prompt shows is the boundary.

## Cross-run state

- Filesystem effects and spawned processes persist across runs; JavaScript state does not. Program-provided data enters the next model context only through agent `facts`.
- A spawned process is referenced by its opaque `id` handle, which crosses runs exactly like a file path: return `{proc, log, ...}` facts, then in a later run read `log` with `host.fs.read(log, from, to)` resuming after the last consumed line, and query `host.proc.status`. A handle from before a restart reports `process_lost`; the log file remains readable.
- Agent `facts` must serialize to at most 16384 bytes. Keep only decision-relevant paths, counts, statuses, and bounded samples. Do not return complete scan results, whole file contents, large arrays, credentials, or unbounded exception text. The direct result limit is 24 KiB; both are hard boundaries, not targets.
- If large data must survive the run, write it to an authorized file and return only its path, count, and short summary. A path in facts is a reference: the next step must call a host API to use that file.
- The host adds bounded status, errors, elapsedMs, and host-derived write receipts to model observations. Do not duplicate them; receipts describe committed writes even when a later operation fails. Use elapsedMs to avoid repeating unnecessarily expensive operations.

## Closed-loop edit shape

When the request mechanically defines candidate scope and the replacement rule, keep discovery, action, and verification in one run:

```run
const root = "/path/from/the-runtime-state"; // replace with the working root from the runtime state
const oldText = "OLD_TOKEN";
const newText = "NEW_TOKEN";
const files = new Set();
let oldCount = 0;
let applied = 0;
let stage = "discover";
let currentPath = null;
try {
  for await (const line of host.fs.scan(root, {contains: oldText})) {
    if (!line.text.includes(oldText)) continue; // the final JavaScript predicate belongs here
    files.add(line.file);
    oldCount += line.text.split(oldText).length - 1;
  }
  if (oldCount === 0) {
    return {to: "model", facts: {phase: "no_match", oldText}};
  }
  stage = "apply";
  for (const path of files) {
    currentPath = path;
    // all: true only when the task rule covers every occurrence in this file
    applied += host.fs.replace(path, oldText, newText, {all: true}).replacements;
  }
  currentPath = null;
  stage = "verify";
  let residual = 0;
  for await (const line of host.fs.scan(root, {contains: oldText})) {
    if (line.text.includes(oldText)) residual += line.text.split(oldText).length - 1;
  }
  if (residual === 0 && applied === oldCount) {
    return {to: "user", message: `Updated ${files.size} files with ${applied} exact replacements.`};
  }
  return {to: "model", facts: {phase: "postcondition_failed", oldCount, applied, residual}};
} catch (error) {
  const message = error && error.message ? error.message : String(error);
  return {to: "model", facts: {phase: "operation_failed", stage, currentPath, applied,
    error: {kind: "filesystem_operation_failed", message: message.slice(0, 240)}}};
}
```

Adapt paths, predicates, exclusions, no-match meaning, and the postcondition to the request. List every file this shape may write in the ` ```access ` block. If candidate scope needs semantic interpretation that cannot be encoded safely, return bounded evidence before changing state; otherwise do not turn discovery results into a model checkpoint.

## Filesystem selection

- Declare every intended write target and command in the access block; a write to an undeclared, unapproved path is a denial at the write call, not a second prompt, and an undeclared command fails at the call with `command_not_authorized`. A denied write or command is final for this run; report it, do not retry alternate spellings.
- `host.fs.list` inspects one directory level. `host.fs.walk` yields one `{file, size}` entry per regular file, for recursive counts, size totals, and path lists. `host.fs.scan` yields one `{file, no, text}` line, for recursive content search. Counting scan yields counts lines, not files. For one known file, use `host.fs.read` or `host.fs.text` instead. `read` is the display channel with stable `N: text` line numbers; never add or parse those prefixes yourself. `text` is the plain whole-file channel for programmatic transformation.
- Walk and scan defaults make routine scope exclusions deterministic: .gitignore is respected and hidden entries are skipped, so build, dependency, and version-control trees (target, node_modules, .git, and similar) are out of scope without a listing step. Pass `skipDirs` only for extra prunes beyond the defaults; do not list a directory to judge routine scope.
- `host.fs.scan` may use `contains` as a Rust-side literal prefilter when that literal is required by every relevant match. JavaScript remains the final predicate for regexes, case rules, multiple conditions, cross-line state, and custom limits.
- `host.fs.replace` is for exact changes. It fails instead of guessing, requires one match by default, and permits `{all: true}` only when every exact occurrence in that file should change. `host.fs.write` is for new files, complete rewrites, or non-exact transformations. Both enforce the current run's write authorization; verify the requested semantic postcondition rather than re-reading solely to confirm a write.

## Process and network selection

- Pick the verb by lifetime: `exec` for anything whose result the current run needs (build, test, lint — no file, no handle); `spawn` for anything that must outlive the run (dev servers, watchers). There is no third lifetime. A run awaiting a spawned server times out and the server keeps running.
- `exec` output is bounded in its return value; spawn output has exactly one durable home, the `log` file. Never manage buffers or choose where output goes: read the log with the `host.fs.read` you already know, resuming from the line after the last one consumed, and distill into facts or an authorized write rather than returning raw output.
- The run deadline bounds the observer, not the observed: `wait` can time out while the process lives. `kill` terminates the whole group and is idempotent. The table holds at most 8 live processes; kill one before spawning another. If the model needs command output inside the project, it distills the log and writes the distillation through the authorized write path.
- `host.net.fetch` changes no local machine state, so it needs no declaration: reading a documentation page, querying an API, or downloading text is one run with `fetch` and a bounded conclusion in facts — no authorization prompt anywhere. A fetch response enters cage memory only; reaching local disk requires the already-authorized write path. Every request is journaled (method, final URL, status, byte count) — egress is detectable after the fact, not prevented. Use `{env: NAME}` header references for credentials; the value resolves host-side and never enters the cage. Large payloads or streaming upload are a declared `curl`, not fetch.

Available APIs:

{{HOST_API}}
</tool_contract>
