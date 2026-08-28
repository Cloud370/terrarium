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

The current filesystem API is intentionally text-oriented. `list`, windowed `read`, bounded `text`, streaming `scan`, and atomic text `write` expose no ambient filesystem API. Scan traversal, open, and decoding failures are returned to the program rather than silently discarded.

## Model data boundary

Content sent through `host.llm` leaves the local process and is disclosed to the configured third-party endpoint. A mounted file is not automatically sent, but a program can read an operator-approved file and include it in a model request.

API keys are read only by the host process from `TERRARIUM_LLM_API_KEY`; they are not injected into JavaScript. The binary does not load `.env` files. Keep secret files outside mounted directories. Provider response bodies are bounded and are not copied into error messages by default.

The built-in vision model declaration does not currently enable image payloads. The implemented LLM path sends text-only chat-completions requests.

## Logs

Stderr may contain model names, paths, timing, error messages, and run source when `TERRARIUM_LOG_RUNS=1`. Do not enable that setting when source or mounted data is sensitive. The project does not yet implement a JSONL session trace, artifact store, or Web UI service; those are future designs and must not be treated as existing security controls.

Report security issues privately to the repository owner until a dedicated disclosure address is published.
