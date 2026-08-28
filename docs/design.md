# Terrarium Design Direction

This document records the small core that is implemented today and the boundaries for future work. It is not a promise that future sections already exist in the binary.

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

The outer agent parser accepts exactly one closed `run` fence per reply. A missing fence, an unclosed fence, or more than one `run` fence is a protocol error; the parser never executes one block and silently ignores the rest. Opening and closing fences must stand alone on their lines. A normal run return is an observation. `host.agent.answer(text)` is the explicit operation that commits the session answer.

The runtime no longer has nested run or sub-agent primitives. JavaScript functions and promises are sufficient for computation and concurrency inside one run; a future independent execution primitive would need its own cancellation, budget, and result contract before being added.

## 3. Outcome and budgets

The library returns a structured `Outcome`:

```text
Outcome
├── ok
├── value             JSON-compatible value | absent
├── answer            session answer | absent
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

- `host.fs.list`, `read`, `text`, `scan`, and `write`;
- `host.llm.call` for stateless nested text requests — the nested model sees only what the program passes, with no contract, mounts, or host capabilities;
- `host.agent.answer` for session completion.

Mounts are the authorization boundary. The operator declares `/virtual=real` for read-only access or `/virtual=real:rw` for writes. Virtual paths are validated component-wise; overlapping virtual mount roots are rejected by the CLI mount parser. Reads and scans do not follow symlinks across the boundary, and writes validate the existing parent chain before creating missing directories.

Scan defaults intentionally resemble ripgrep: `.gitignore` is respected, hidden entries and binaries are skipped, symlinks are not followed, and options are explicit. Traversal, opening, and decoding errors are observable rejections rather than empty streams.

## 5. LLM configuration and capabilities

The implemented configuration is one explicit OpenAI-compatible chat-completions connection:

- `TERRARIUM_LLM_API_KEY`;
- `TERRARIUM_LLM_BASE_URL`;
- `TERRARIUM_LLM_MODEL`;
- `TERRARIUM_LOG_RUNS`.

The binary does not load `.env` files. The endpoint, credential, and model are process configuration, not separate runtime objects.

The built-in model examples declare:

```text
deepseek-v4-flash             text -> text
deepseek-v4-flash-vision-exp  text,image -> text
```

This phase only declares image capability. The request payload remains text-only; image file reads, encoding, content parts, and artifact storage are not implemented.

## 6. Public boundary

`Kernel` and validated `Mount` are the reusable library API. The CLI and outer agent loop are adapters. The agent loop currently prints terminal-oriented output and is not yet a service/session API.

## 7. Explicit future work

The following are deliberately not part of the current contract:

- TOML configuration files and a provider/model catalog;
- reasoning-level controls;
- multimodal request payloads and image transport;
- artifacts and binary result storage;
- JSONL trace events and replay;
- a Web UI or HTTP service;
- child runs or sub-agent sessions — a sub-agent is a controlled sub-session and must, before becoming a capability, define its own conversation history, round and per-run timeout budgets, total budget, cancellation, mounts that only inherit or narrow the parent's, structured result, explicit lifecycle state, and a recursion cap on further spawning;
- token or cost hard budgets;
- transactional effects or automatic rollback.

When any of these becomes necessary, it should be introduced behind a small typed boundary and added to the registry, prompt, tests, and documentation together. The current core should not grow speculative abstractions before a concrete consumer exists.
