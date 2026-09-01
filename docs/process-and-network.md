# Process Execution and Network Fetch

Status: implemented contract. This document supersedes the future-process/network speculation formerly in section 11 of `filesystem-authorization.md`; that section now points here. `host.proc` (`exec`, `spawn`, `status`, `wait`, `kill`), `host.net.fetch`, the access block's `commands` field, and the `run/spawn`, `proc/exit`, and `net/request` journal events are installed in the current binary. The implementation lives in `src/proc.rs`, `src/net.rs`, and the command lifecycle in `src/auth.rs` and `src/agent.rs`.

## 1. Scope

This proposal installs two capabilities next to `host.fs`:

- `host.proc` — run external commands: `exec`, `spawn`, `status`, `wait`, `kill`;
- `host.net` — `fetch`, an HTTP client.

The one-line trust model is unchanged from the filesystem proposal: **changing the local machine requires consent; everything else leaves a record.** Writes and process creation are approved before QuickJS starts; network requests change no local state, run without consent, and are journaled per request.

## 2. Mental model

- The cage never owns long-lived things; it references them. A process is host-owned session state, referenced by an opaque handle exactly as a file is referenced by a path.
- The run deadline bounds the observer, not the observed: a run awaiting a server that never exits times out, and the server keeps running.
- The only lifetime distinction the model manages is the verb it picks: `exec` is run-scoped, `spawn` is session-scoped. There is no third lifetime.
- Process output has exactly one durable home: a host-owned log file. The model never chooses where output goes, never manages buffers or cursors, and reads logs with the `host.fs.read` it already knows.

## 3. First principles

1. The model proposes commands and write targets; it never grants itself authority.
2. A command is a structured record — resolved executable plus an argument array — never a command-line string. There is no shell in the spawn path, so there is no injection surface and no parsing ambiguity.
3. One user decision covers the whole requested set for one run. Partial approval does not exist.
4. Approving a command approves every effect of that process for its remaining lifetime. That is strictly stronger than approving any set of file writes — a child process is not bound by Terrarium's write scopes — so the quality of the approval display is the real security boundary.
5. Output needs one channel, not three. A host-owned append-only log replaces ring buffers, cursors, and overrun flags; the existing windowed, line-numbered `host.fs.read` is the drain protocol.
6. The journal records decisions and receipts, never stream data. Historical text never restores authority; a session table never survives a restart.
7. `fetch` changes no local machine state and therefore needs no consent, but it is an unguarded egress channel for everything the operating-system user can read. The journal is detection, not prevention, and the contract says so.
8. No capability ships without a complete contract for its limits and its failure modes.

## 4. `host.proc`

The namespace follows `host.fs`: flat module functions that take explicit references and return plain records. There are no handle objects with methods, because no existing host capability has them, because plain records serialize into facts, and because one-line signatures stay reviewable in the registry.

Two names were deliberately avoided. `run` is reserved: in this project a *run* is the cage execution unit (`run` fence, `run/start`, `run/result`), and reusing the word for a child process would blur the protocol's most central term. And `attach` is avoided because a later run does not acquire a live connection — it reads a record and a file.

### 4.1 `exec` — run-scoped one-shot

```js
const r = await host.proc.exec("cargo", ["test", "--", "--nocapture"], {cwd: "/code/app"});
// r: {code, stdout, stderr}
```

- Waits for completion within the current run. `stdout` and `stderr` are captured separately, each bounded to 16 KiB as head-plus-tail with an omitted-byte count in the middle.
- If the run ends first — deadline, cancellation, or failure — the host kills the child's process group. `exec` leaves no file; its captured result is the only output channel, and the journal receipts are the only trace after the run is gone.
- This is the verb for build, test, lint, and every command whose result the current run needs.

### 4.2 `spawn` — session-scoped process

```js
const p = await host.proc.spawn("npm", ["run", "dev"]);
// p: {id, log, output}
```

- `id` is an opaque session-scoped handle (`"p1"`, `"p2"`, ...). It is the only reference that crosses runs, and it travels in facts exactly like a file path.
- `log` is an absolute path to a host-owned append-only file under the session state directory (`.../terrarium/sessions/<sid>/procs/<id>.log`). Standard output and standard error share one interleaved timeline, because correlating them is the debugging need and piping them apart is a non-goal. The file is capped at 4 MiB; on reaching the cap the host stops appending and writes one final marker line. The head is never rewritten, so line numbers are stable forever.
- `output` is a live async-iterable view of what `log` accumulates, yielding `{no, text}` — the `host.fs.scan` idiom. It exists only inside the spawning run; later runs read `log` with `host.fs.read(log, from, to)`, where `from` is the line after the last one they consumed. The live iterator and the file are two views of one stream, not two channels.
- This is the verb for anything that must outlive the run: dev servers, watchers, daemons.

### 4.3 `status`, `wait`, `kill`

```js
host.proc.status(id)            // -> {id, log, running, code}
await host.proc.wait(id)        // -> final record; bounded by the run deadline
host.proc.kill(id)              // -> final record; graceful termination of the process group
host.proc.kill(id, {force: true})
```

- `status` looks up the session's in-memory table only. An unknown or pre-resume handle is the error `process_lost` — the table is not durable and historical journal text is not authority.
- `wait` blocks until exit, within the current run's deadline. When the deadline fires, the run dies and the process does not.
- `kill` terminates the whole process group (Unix) or job (Windows): graceful by default, forced with `{force: true}`. Killing an already-exited process is idempotent and returns its final record.
- Dead entries stay in the table for post-mortem `status`/`kill` queries. The table holds at most 8 live processes and 16 entries total (LRU over dead ones); when full, `spawn` fails with a visible error. The host never silently kills an old process to make room.

### 4.4 Deliberately absent

No `stdin` writes (an interactive interpreter plus stdin injection would make the approved argv an incomplete summary of what executes; it returns when a real consumer workflow exists, with its own authorization questions answered). No environment-variable setting by the model. No signal sending beyond kill. No piping between processes. See section 11.

## 5. Lifetime and ownership

The host session owns the process table, in memory. Children are created in their own process group (Unix) or assigned to a kill-on-close job object (Windows).

| Event | Effect on the process |
|---|---|
| run returns or fails | none |
| run times out or is killed | `exec` child dies with its run; `spawn`ed processes unaffected |
| explicit `kill` | process group terminated |
| session ends normally | all live processes killed; logs remain as ordinary session files |
| host crashes or is killed | best effort: job objects kill Windows children; on Unix, Linux `PDEATHSIG` where available — otherwise the process may be orphaned |
| session resumes | table is gone; old handles report `process_lost`; logs remain readable as files |

Crash cleanup is stated honestly rather than guaranteed: after a Unix crash outside Linux, a spawned process can survive its session. The journal records each process's pid, so the user can reap stragglers manually. What is guaranteed is the complement: resume never resurrects a process, and no historical text ever acts as authority.

## 6. Output model

| Data | Channel | Bound | Lifetime |
|---|---|---|---|
| one-shot result | `exec` return value | 16 KiB per stream, head+tail | the run |
| durable timeline | spawn log file | 4 MiB, then a truncation marker | the session's files |
| distilled conclusions | `facts` | 16 KiB | the journal |

The model chooses a verb, not a storage strategy. Reading a log is the same skill as reading a source file: `host.fs.read(log, 120, 180)` returns numbered lines; a later run resumes from the next line number. A truncation marker is a visible line, not a silent gap. If the model wants command output inside the project, it distills the log and writes the distillation through the authorized write path — the facts discipline, enforced by structure rather than by advice.

The journal never stores stream data. It records three receipts: `run/spawn` at creation (resolved executable, argv, cwd, pid, handle, log path), `proc/exit` at exit (handle, exit code, a ~1 KiB tail), and `net/request` per fetch.

## 7. `host.net.fetch`

```js
const res = await host.net.fetch("https://api.example.com/repos/x", {
  method: "GET",
  headers: {Authorization: {env: "GITHUB_TOKEN"}},
});
// res: {status, finalUrl, body}
for await (const chunk of res.body) { /* strings, lossy UTF-8 */ }
```

- Any method (`GET`, `HEAD`, `POST`, `PUT`, `PATCH`, `DELETE`), any http/https URL, executed as the operating-system user — the same trust decision as filesystem reads inheriting the OS-readable view.
- Header values are literal strings or `{env: NAME}` name references resolved host-side; credential values never enter the cage. URLs with userinfo or fragments are rejected as syntax, not as authorization.
- Redirects are followed (at most 5) and the final URL is journaled. No cookies, no cache.
- Physical limits are host-owned: 60 s per request covering the response head and its body consumption, an 8 MiB response-body cap that rejects with a visible error when exceeded, at most 4 concurrent requests, CRLF rejection in header names and values, and an 8 KiB request-URL cap. `--offline` disables the capability for the whole invocation.
- Every request is journaled as `net/request` with method, final URL, status, and byte count. A request that fails, times out, or is cancelled after dispatch is journaled with status 0 — bytes may have left the machine. Receipt batches are capped per journaling pass, and a `receipts/truncated` marker counts any dropped receipts so the audit trail never truncates silently.

Why no consent: a fetch response enters cage memory only; reaching local disk requires the already-authorized write path, so the local-mutation loop is closed by construction. The egress loop is *not* closed: anything the OS user can read can be sent anywhere in one zero-consent request, and the journal detects this after the fact rather than preventing it. Selecting a model provider already makes the same trade for read data; this proposal states the consequence instead of implying safety. Operators who need prevention run `--offline` or an egress firewall — a host concern, not a cage capability.

## 8. Authorization

### 8.1 The access block grows one field

```json
{"writes": [],
 "commands": [{"exe": "cargo", "argv": ["test", "--", "--nocapture"], "cwd": "/code/app"}],
 "reason": "Verify the fix passes"}
```

The empty form stays unconditional: `{"writes":[],"commands":[],"reason":""}`.

- `commands` is an array of at most 8 records; each record is `{exe, argv, cwd?}`. `cwd` defaults to the session working root. The whole block stays within an 8 KiB encoded bound and the 200-character reason limit; per-argument display truncates long elements with a marker while the journal keeps the exact record.
- `exe` is resolved by the host (PATH lookup, symlink normalization) at approval time and again at call time; matching is resolved-identity equality plus element-wise argv equality plus cwd equality. The journal records the resolved path; declarations are deduplicated after resolution.
- Both `exec` and `spawn` check against the same records. A call that matches no declared record fails with `command_not_authorized`, and the error prints the full expected record so the next run can correct itself.
- The request is run-local, decided as one set, never partially approved, and re-declared every run.

### 8.2 Modes

| Mode | Writes | Commands | Fetch |
|---|---|---|---|
| `read-only` | denied | denied; a declaration is corrective feedback | allowed, journaled |
| `planned-write` | one decision per run | one decision per run | allowed, journaled |
| `full-access` | no prompt, journaled | no prompt, journaled | allowed, journaled |

Process creation is a write-class effect — a child is not bound by write scopes — so `read-only` denies it. `full-access` drops the prompts but keeps every receipt; it means the operator accepted the machine-level identity of the OS user, for every installed capability.

### 8.3 Operator pre-grant

`--allow-exec NAME` (repeatable, `planned-write` only) matches the resolved executable only, any argv, covering both `exec` and `spawn`; covered records never reach a prompt, exactly as `--allow-write` scopes subtract write targets. The declaration habit is unchanged: the model always declares, the host subtracts.

One honest warning belongs in the operator docs: an executable that loads project code — build tools (`cargo`, `npm`, `make`) as much as interpreters (`sh`, `node`, `python`) — turns the workspace into its program. Since the model can write into the workspace through authorized writes, allowing such an executable approaches full trust for it. There is no blacklist; the display and this rule are the defense.

### 8.4 Display quality

The approval prompt renders each command as the exact argv with the executable resolved, the working directory, and the reason — the same "what you read is what runs" guarantee the filesystem proposal gives for writes. With no shell, no environment setting, and no stdin in v1, the approved record is a complete summary of what will start. This, not any downstream check, is the boundary; downstream errors like `command_not_authorized` only keep the declaration honest.

## 9. Contract, runtime state, and migration

- The stable prompt prefix moves to its next version: the "no process, no network" sentences are replaced by the two capability descriptions; material is appended in deterministic order; existing prefix bytes within a version never change.
- The runtime-state block gains three lines — `Platform`, `Live processes` (handles and executables of live processes, capped to one line, never output), and the capability list gains `host.proc`, `host.net`. The spawn-log directory is host-owned state, is not rendered as a writable root, and needs no model-visible prefix because the model never constructs log paths.
- Session validators must accept old two-field `run/access` events (missing `commands` reads as empty) so existing journals replay unchanged; new events (`run/spawn`, `proc/exit`, `net/request`) are validated like every other event, unknown fields rejected.
- `registry.rs` remains the single source of truth for the model-visible surface; the two capabilities exist only once their registry lines, contract text, prelude shims, and validators land together.

## 10. Model guidance (QuickJS is not Node)

The prelude installs throwing shims for `require`, `process`, and `Buffer` with a message naming the host replacements, and never polyfills capability-shaped globals; cheap spec pieces (`TextEncoder`) remain. There is no bare `fetch` global alias. The contract teaches one triage: pure computation stays in JavaScript; data access uses `host.fs` and `host.net.fetch`; a real toolchain is a declared command. `sh` is only for composing several real tools, never for wrapping one.

Errors are the curriculum: the namespace proxy reports the available surface, `command_not_authorized` reports the expected record, `process_lost` reports that a handle predates the current session. Each is designed for self-correction in the next run.

## 11. Non-goals

Pipes and multi-process composition; PTY/TTY and inherited stdio; `stdin` writes; signals beyond kill; model-set environment variables; raw TCP, DNS, TLS, or proxy configuration from the cage; streaming upload (large payloads are a spawned `curl`); SSE and WebSocket in the cage (the transport layer's job in `src/llm/`); filesystem deletion; and network authorization — the escape hatch, if a real consumer ever appears, is a `requests` field in a future access-block version, designed then, not now.

## 12. Minimal verification workflows

1. **Build/test**: one run, declare `cargo test`, `exec` it, summarize pass/fail counts in facts. No file, no handle.
2. **Dev server**: run A declares and spawns `npm run dev`, iterates `output` until the port line, returns facts `{proc, log, url}`. Run B (fresh cage) reads `log` from the next line and checks `status`. Run C kills. Resume path: handles report `process_lost`, the log still reads.
3. **Fetch docs**: one run, `fetch` a page, filter the body stream in JavaScript, return a bounded conclusion in facts — no authorization prompt anywhere.

## 13. Implementation map

The implementation follows this direction: `src/proc.rs` (process table, tokio `Command`, process groups and job objects, log writer with the truncation marker); `src/net.rs` (`fetch`, reusing the llm transport's HTTP client); `auth.rs` command-record parsing, resolution, and freezing; `agent.rs` access-block extension; `registry.rs` capability lines; `prelude.js` shims plus streamification of `output` and `body`; contract text update and session-validator back-compat (old two-field `run/access` events replay unchanged); `filesystem-authorization.md` §11 (both languages) points here.
