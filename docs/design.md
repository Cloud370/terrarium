# Terrarium Design Direction

This document records the small core that is implemented today and the boundaries for future work.

## 1. Core model

Terrarium is an agent kernel: the model submits a bounded program, the program composes host capabilities, and the kernel executes it with resource limits before returning a structured result.

```text
model reply
  -> closed run fence
  -> fresh QuickJS cage
  -> host capabilities under operator mounts
  -> structured Outcome
```

JavaScript is the current program language. The `run` fence belongs to the outer text adapter; the reusable kernel receives source text and returns `Outcome`.

## 2. Execution contract

Every source is evaluated as one async function body. This gives one rule everywhere:

- top-level `return` supplies the current run's `value`;
- top-level `await` is legal;
- no `return` means `value` is absent;
- there is no last-expression fallback;
- source shape is never inspected to choose script versus function semantics.

The outer agent parser accepts exactly one closed `run` fence per reply. A missing fence, an unclosed fence, or more than one `run` fence is a protocol error; the parser never executes one block and silently ignores the rest. Opening and closing fences must stand alone on their lines. In agent mode, a successful program must return exactly `{to: "model", facts: {...}}` or `{to: "user", message: "..."}`. The first ends the current run and continues the same turn; the second ends the turn and hands control to the user. Format and recoverable operation errors are model observations, not automatic handoffs. Direct `terrarium run` keeps ordinary JSON-compatible return values.

The runtime no longer has nested run or sub-agent primitives. JavaScript functions and promises are sufficient for computation and concurrency inside one run; a future independent execution primitive would need its own cancellation, budget, and result contract before being added.

## 3. Outcome and budgets

The library returns a structured `Outcome`:

```text
Outcome
├── ok
├── value             JSON-compatible value | absent
├── stdout
├── error             {kind, message} | absent
├── termination       returned | failed | timed_out | cancelled | fatal
├── timed_out
└── elapsed_ms
```

The current hard boundaries are 64 MiB QuickJS heap, 1 MiB stack, a 300-second maximum run timeout, 16 KiB captured stdout, bounded file reads, bounded scan lines, and a finite outer-agent round budget. LLM response bodies are capped before JSON parsing. Token counts and cache counters are informational process-lifetime statistics, not session budgets.

A fresh runtime per run keeps a failed or timed-out program from poisoning the next turn. The library never exits the process; exit codes belong to the CLI adapter.

## 4. Host capabilities

The registry is the single source for the generated contract. The current surface is intentionally small:

- `host.fs.list`, `read`, `text`, `scan`, `walk`, and `write`. `scan` and `walk` share one traversal engine: scan streams a tree's lines, walk streams its file entries.
- The tagged agent return protocol: `to: "model"` for same-turn continuation and `to: "user"` for an explicit user handoff. The main model request is performed by the trusted outer loop; JavaScript has no nested model-call primitive.

Mounts are the authorization boundary. The operator declares `/virtual=real` for read-only access or `/virtual=real:rw` for writes. The default model-driven agent binds its session to the current working root; `terrarium run` uses the current directory transiently. Agent invocations select `workspace` by default, or `--read-only` or `--full-access` for that invocation only. Explicit `--mount` entries apply to every run in that invocation. `--full-access` maps `/` to the real filesystem view of the current operating-system user. These modes and mounts are never stored in the journal.

Scan defaults intentionally resemble ripgrep: `.gitignore` is respected, hidden entries and binaries are skipped, symlinks are not followed, and options are explicit. Traversal, opening, and decoding errors are observable rejections rather than empty streams.

## 5. LLM configuration and capabilities

The implemented configuration is a strict TOML document containing named providers and profiles. A provider supplies an HTTP(S) base URL and an optional credential environment-variable name; a profile selects the built-in `openai-chat-completions` protocol, exact model ID, and optional output-token and reasoning-effort settings. Each turn stores its resolved non-secret profile and exact system prompt.

When no TOML file is selected, the legacy `TERRARIUM_LLM_API_KEY`, `TERRARIUM_LLM_BASE_URL`, and `TERRARIUM_LLM_MODEL` variables remain a compatibility fallback. The binary does not load `.env` files.

The built-in model examples declare:

```text
deepseek-v4-flash             text -> text
deepseek-v4-flash-vision-exp  text,image -> text
```

This phase only declares image capability. The request payload remains text-only; image file reads, encoding, content parts, and artifact storage are not implemented. The transport performs no hidden retry.

## 6. Durable sessions

Agent sessions use one append-only JSONL file under the per-user state directory. The header binds the session to its absolute display and canonical working root. Each turn stores the user message, exact system prompt, resolved profile, and limits. Model requests and JavaScript runs are written before network dispatch or execution; an uncertain run is recorded as `outcome_unknown` and never replayed. Access mode belongs only to the current invocation.

`Kernel` and validated `Mount` are the reusable library API. The CLI, durable session journal, and outer agent loop are adapters over that core. The default command runs the model-driven agent; direct JavaScript is intentionally isolated behind `terrarium run`.

## 7. Explicit future work

The following are deliberately not part of the current contract:

- multimodal request payloads and image transport;
- artifacts and binary result storage;
- a Web UI or HTTP service;
- child runs or sub-agent sessions — a sub-agent is a controlled sub-session and must, before becoming a capability, define its own conversation history, round and per-run timeout budgets, total budget, cancellation, mounts that only inherit or narrow the parent's, structured result, explicit lifecycle state, and a recursion cap on further spawning;
- token or cost hard budgets;
- transactional effects or automatic rollback.

When any of these becomes necessary, it should be introduced behind a small typed boundary and added to the registry, prompt, tests, and documentation together. The current core should not grow speculative abstractions before a concrete consumer exists.
