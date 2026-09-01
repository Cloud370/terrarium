# Security and Trust Boundary

Terrarium embeds QuickJS and gives JavaScript only explicitly installed `host.*` capabilities. Each run has a fresh runtime with heap, stack, stdout, file-read, response-body, process-capture, and deadline limits.

This is a language-runtime cage, not an operating-system security boundary for hostile multi-tenant execution. The process does not add a container, seccomp policy, namespace boundary, or separate worker. A vulnerability in QuickJS, its Rust binding, or a trusted host capability would remain within the host process boundary.

## Filesystem modes

There is no virtual path namespace. Every path a program uses is one absolute path in the operating-system user's filesystem view; reads see exactly what the current OS user can read. Writes and process creation are governed by one invocation-selected mode:

| Mode | Reads | Writes | Process launch |
|---|---|---|---|
| `read-only` | OS-readable absolute paths | every write denied (`write_denied`) | denied (`command_not_authorized`) |
| `planned-write` (agent default) | OS-readable absolute paths | targets preauthorized per run (`write_not_authorized` otherwise) | commands preauthorized per run (`command_not_authorized` otherwise) |
| `full-access` | OS-readable absolute paths | any validated path, subject only to OS permissions | any command, journaled, no prompt |

The mode is selected by the trusted invocation — `--read-only`, `--full-access`, `--allow-write DIR\|FILE`, `--allow-exec NAME`, `--offline` — never by the model, never stored in the session, and never restored from the journal.

## Write preauthorization

In `planned-write` mode each run's writes are frozen before QuickJS starts:

1. The model response may carry one `access` block declaring at most 32 exact absolute file paths plus a reason (protocol details in [protocol.md](protocol.md)).
2. The host parses and validates every path: no relative paths, `.`, `..`, ambiguous `//`, globs, directories, or existing symbolic-link targets; parent aliases collapse by canonicalization; identities must be unique after normalization.
3. Targets already covered by an operator `--allow-write` scope never prompt.
4. The remaining set is presented as one decision to the `Authorizer` — allow or deny the whole set. Partial approval does not exist. A pipe, CI job, or background run has no interactive authorizer and reports `unavailable`; no write is authorized.
5. On approval the run's authority is frozen: operator scopes plus the approved exact paths, in canonical identity form.
6. Every actual write during the run re-resolves the target identity and checks it against that frozen scope. Alternate spellings and symlink traversal cannot widen it. Approving a new file includes creating its missing parent directories; existing symbolic links and directories are never written.

`--allow-write DIR` grants a recursive prefix, `--allow-write FILE` one exact file, resolved to canonical identity at launch. Operator arguments are trusted host input; model-declared targets are not. `--allow-write` cannot be combined with `--read-only` or `--full-access` (startup error), and it never enables writes in `full-access`'s place — `full-access` simply has no scope check beyond path validation.

Each request and decision is journaled as one bounded `run/access` event, including declarations accepted and ignored under `full-access`. The journal is an audit record, never authority: resuming a session reconstructs nothing from it, and approvals are discarded when their run ends.

The current filesystem API is intentionally text-oriented. `list`, windowed `read`, bounded `text`, streaming `scan`, entry-streaming `walk`, atomic text `write`, and targeted `replace` expose no ambient filesystem API. Scan traversal never follows symlinks; open and decoding failures are returned to the program rather than silently discarded; `walk` never opens files, so it carries only traversal failures.

The runtime does not claim to defeat a hostile concurrent process that replaces filesystem objects between the identity checks and the write; that requires an operating-system isolation boundary outside this design.

## Process preauthorization

Process execution follows the same one-decision lifecycle as writes, specified normatively in [process-and-network.md](process-and-network.md). A command is a structured record — resolved executable, exact argv array, cwd — never a command-line string, and there is no shell in the spawn path. `read-only` denies process creation outright; `planned-write` freezes the approved command records next to the write scopes before QuickJS starts; `full-access` runs any command without a prompt but keeps every receipt.

Approving a command approves every effect of that process for its remaining lifetime: a child process is not bound by Terrarium's write scopes, so the quality of the approval display — the exact argv with the executable resolved, plus the working directory and reason — is the real security boundary, not any downstream check. `--allow-exec NAME` (repeatable, `planned-write` only) pre-grants an executable by resolved identity for any argv. An executable that loads project code — build tools as much as interpreters — turns the workspace into its program; since the model can write into the workspace through authorized writes, allowing such an executable approaches full trust for it. There is no blacklist; the display and this rule are the defense.

Process lifetime: `exec` children die with their run (deadline, cancellation, or failure kills the process group); `spawn`ed processes outlive runs, are killed on normal session end, and are best-effort reaped after a host crash (Linux `PDEATHSIG`, Windows job objects; on other Unixes an orphan can survive its session — the journal records each pid so the user can reap stragglers). The process table is host memory only: resume never resurrects a process, and no historical journal text ever acts as authority.

## Network egress

`host.net.fetch` changes no local machine state, so it needs no consent in every mode; a response enters cage memory only, and reaching local disk requires the already-authorized write path. The egress loop is *not* closed: anything the operating-system user can read can be sent anywhere in one zero-consent request. The journal (`net/request` receipts: method, final URL, status, byte count) detects this after the fact rather than preventing it. Operators who need prevention run `--offline` — which disables fetch for the whole invocation — or an egress firewall; that is a host concern, not a cage capability.

## Model data boundary

Content sent through the main model request leaves the local process and is disclosed to the provider selected by the current turn's resolved profile. A readable file is not automatically sent, but a program can read any OS-readable file and include it in the next observation or model-visible context. `--read-only` and `planned-write` bound writes, not reads; use the operating-system user's own permissions to bound reads.

API keys are read only by the host process from the environment variable named by the selected profile; they are not injected into JavaScript. The resolved profile stores only that variable name, never its value. The binary does not load `.env` files. Provider response bodies are bounded and are not copied into error messages by default. Fetch header values may be `{env: NAME}` references, resolved host-side by the same rule: the name crosses into the cage, the value never does.

JavaScript has no `host.llm.call` primitive. The outer model loop is the only model-call path, and each attempt is durably recorded before dispatch. Requests are text-only.

## Sessions and authority

Agent sessions are append-only JSONL files containing prompts, resolved non-secret profiles, model observations, run boundaries, access decisions, process and network receipts, and answers. They may contain user prompts, source code, paths, model responses, program output, and answers. A session binds to its stored working root for display, but the journal is not an authorization token and contains no authority: each invocation selects a fresh mode, and the current host policy is applied when a run executes, including during recovery. A durable `run/start` without a result is recorded as unknown and is never replayed.

## Logs

Stderr may contain model names, paths, timing, error messages, and the newly created session ID. Protect the per-user state directory because the journal is ordinary application state and is not encrypted, signed, or redacted. Direct-run JSON output is an adapter result, not a durable trace.

Report security issues privately to the repository owner until a dedicated disclosure address is published.
