# Security and Trust Boundary

Terrarium embeds QuickJS and gives JavaScript only explicitly installed `host.*` capabilities. Each run has a fresh runtime with heap, stack, stdout, file-read, response-body, and deadline limits.

This is a language-runtime cage, not an operating-system security boundary for hostile multi-tenant execution. The process does not add a container, seccomp policy, namespace boundary, or separate worker. A vulnerability in QuickJS, its Rust binding, or a trusted host capability would remain within the host process boundary.

## Filesystem authorization

Filesystem access exists only under operator-declared mounts:

```text
--mount /proj=/real/path       read-only
--mount /proj=/real/path:rw    read-write
```

Mounts are validated before a run starts. Virtual paths reject ambiguous components and lexical escapes. Existing parent directories are canonicalized before writes create missing directories. Scans do not follow symlinks. A read-only mount is a policy boundary; the program cannot promote it to writable access.

The current filesystem API is intentionally text-oriented. `list`, windowed `read`, bounded `text`, streaming `scan`, entry-streaming `walk`, and atomic text `write` expose no ambient filesystem API. Scan traversal, open, and decoding failures are returned to the program rather than silently discarded; `walk` never opens files, so it carries only traversal failures.

## Model data boundary

Content sent through the main model request leaves the local process and is disclosed to the provider selected by the current turn's resolved profile. A mounted file is not automatically sent, but a program can read an operator-approved file and include it in the next observation or model-visible context.

API keys are read only by the host process from the environment variable named by the selected profile; they are not injected into JavaScript. The resolved profile stores only that variable name, never its value. The binary does not load `.env` files. Keep secret files outside mounted directories. Provider response bodies are bounded and are not copied into error messages by default.

JavaScript has no `host.llm.call` primitive. The outer model loop is the only model-call path, and each attempt is durably recorded before dispatch.

The built-in vision model declaration does not currently enable image payloads. The implemented LLM path sends text-only chat-completions requests.

## Sessions and access modes

Agent sessions are append-only JSONL files containing prompts, resolved non-secret profiles, model observations, run boundaries, and answers. They may contain user prompts, source code, paths, model responses, program output, and answers. A session binds to its stored display and canonical working root, but the journal is not an authorization token and contains no access mode.

Each invocation selects `workspace` by default, or `--read-only` or `--full-access`. An explicit `--mount /virtual=real[:rw]` remains installed for the entire invocation, including all runs and recovery. `--full-access` installs `/ -> /` and therefore permits real absolute paths visible to the current operating-system user, but it is not root access. The current host policy is applied when a run executes, including during recovery; resuming a session never restores permissions from journal data. A durable `run/start` without a result is recorded as unknown and is never replayed.
## Logs

Stderr may contain model names, paths, timing, error messages, and the newly created session ID. Protect the per-user state directory because the journal is ordinary application state and is not encrypted, signed, or redacted. Direct-run JSON output is an adapter result, not a durable trace.

Report security issues privately to the repository owner until a dedicated disclosure address is published.
