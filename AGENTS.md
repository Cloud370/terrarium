# AGENTS.md

Terrarium is an agent runtime where the model's actions are JavaScript programs, not tool calls: each run executes in a fresh QuickJS cage (64 MB heap, 1 MB stack, hard deadline) and returns one JSON disposition. Rust workspace, single crate, edition 2021.

## Commands

```sh
cargo build --release                                  # build the binary
cargo test                                             # run the test suite
cargo fmt --all                                        # format (CI enforces --check)
cargo clippy --all-targets -- -D warnings              # lint (CI enforces; zero warnings allowed)
```

- Rust stable ≥ 1.87 (`rquickjs` requires it).
- CI runs `fmt --check` + `clippy -D warnings`, then `test` + `build` on Ubuntu, macOS (arm64 + intel), and Windows. Core behavior must work identically on all three — no platform-specific external commands in core code.
- Windows cross-compile from Linux: `cargo build --release --target x86_64-pc-windows-gnu` (mingw-w64 linker configured in `.cargo/config.toml`).
- `cargo check` gives a fast typecheck; `clippy` is the enforced gate.

## Efficiency hints

Hints, not requirements — apply whichever fit the tools you actually have.

- Locate, don't load. Use whatever content search is available — a search tool, shell `grep`, an editor jump — to find the region that matters, then read just that region. File heads carry doc comments and structure, tails carry tests; often one of the two is enough. Whole-file reads are worth it only for small files or a first structural survey.
- Batch independent actions when your environment allows it: multiple tool calls in one turn, or shell steps chained into one invocation, beat paying a round trip per action.
- Keep reformatting for the end. A formatter rewrites line numbers; run it mid-task and every location already read goes stale, so later edits miss their anchor and force re-reads. Write normally as you go and format once before finishing.
- Verify once at the end — `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` — not after every change.

## Architecture

Library core with thin presentation adapters:

- `src/kernel.rs` — cage boundary: one fresh QuickJS cage per run, resource limits, `eval_js`. `src/lib.rs` exports `Kernel`, `RunFilesystemAuthority`, `Outcome` etc. for non-CLI callers.
- `src/agent.rs` — outer agent loop, access/run fence parser (at most one `access` plus one `run` fence per model response), and the write-preauthorization lifecycle (`Authorizer` decision, frozen authority).
- `src/fs.rs` — host filesystem capabilities over absolute OS-user paths and the frozen `RunFilesystemAuthority` write check. `host.fs.walk` and `host.fs.scan` share one traversal engine. `src/auth.rs` — access-block parsing/resolution, the `Authorizer` trait, operator-scope subtraction and freezing.
- `src/llm/` — streaming model transport for three wire protocols: `openai-chat-completions`, `openai-responses`, `anthropic-messages` (SSE in `sse.rs`, reasoning replay per protocol shape).
- `src/session.rs` — durable append-only JSONL sessions; uncertain runs are never replayed.
- `src/config.rs` — TOML model profiles.
- `src/registry.rs` — **single source of truth for the model-visible host API**. The prompt contract's `{{HOST_API}}` section is generated from `HOST_API` there. Add new host capabilities in `registry.rs` plus the implementation, never by editing the rendered contract text. The system prompt is byte-stable across invocations; per-invocation facts (working root, filesystem mode, timeouts, capabilities) travel only in the `<terrarium-runtime-state>` block prepended to user messages.
- `src/main.rs` / `src/cli.rs` — process and terminal adapters only. Reusable behavior belongs in the library; a future Web UI should build a service adapter over the library, not spawn the binary.
- `src/prompts/` and `src/runtime/prelude.js` — compiled into the binary via `include_str!`. Treat them as code: editing them changes runtime behavior.
- `tests/library_api.rs` — public library API tests.

## Reasoning discipline

Every change starts from first principles, not from an API shape or an implementation pattern. Before adding a capability, module, or boundary, answer these questions in order:

1. What user outcome does this enable, and what is the smallest workflow that proves it?
2. Which facts and effects must cross the boundary being changed, and which are only temporary computation?
3. Who owns each piece of state — who may change it, and when does it begin and end?
4. What is the smallest explicit interface that makes that ownership and lifecycle visible?
5. What happens on failure, timeout, cancellation, process loss, restart, partial completion, and denied permission?
6. Which data belongs in a working context, which in durable state, and which must remain outside both?
7. Can existing boundaries express the workflow? If so, compose them instead of adding a new abstraction.

Ground rules:

- Keep control flow separate from data flow: a result says who acts next; large or sensitive data crosses a boundary only by an explicit, bounded reference.
- Facts on the trusted side of a boundary are derived there, never accepted from the untrusted side.
- Optimize for the fewest steps that establish correctness.
- Introduce no lifecycle, storage layer, routing mechanism, or capability without a concrete consumer and a complete contract for its limits and recovery.
- The design is good when behavior can be reconstructed from its boundaries — what persists, what is released, who acts next, how uncertainty is handled — without reading hidden implementation details.

## Invariants (do not break)

- Each run executes in a fresh cage; runs share no mutable state. Credentials stay in the host process environment, referenced by env-var name in config, and never enter the cage.
- Security lives in host code: filesystem modes (`read-only` / `planned-write` / `full-access`), frozen write scopes and preauthorization before QuickJS starts, symlink/path-escape rejection, resource limits, cancellation. Prompts describe behavior; they never provide the boundary.
- Model requests are made by the trusted outer agent loop and journaled in the session; JavaScript programs cannot call a model.
- Capabilities stay explicit, minimal, typed, bounded, and observable; errors surface at the boundary instead of falling back silently.
- Model-facing data flows into the next step only through `to: "model"` `facts` (16 KiB limit) and bounded host-derived evidence (24 KiB result limit). These limits are hard boundaries, not targets.

## Documentation

Maintained specs live in `docs/` and are the project contract — update them in place when behavior changes: `protocol.md` (wire/execution protocol), `configuration.md` (TOML profiles), `security.md` (trust boundary), `model-profiles-and-durable-sessions.md`, `web-ui.md` (integration boundary). Read the relevant doc before touching the agent loop, protocol, session format, or host API surface.

## Conventions

- Commit messages: `feat:`, `refactor:`, or plain imperative summaries.
- Keep PRs focused; CI must be green (fmt, clippy, test, build).
- README.md and README.zh-CN.md are maintained in parallel — update both for user-facing changes.
