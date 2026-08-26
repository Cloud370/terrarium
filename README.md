# terrarium

**An LLM's actions are programs, not tool calls.** Each turn the model writes one complete ES2020 JavaScript program in a fenced `run` block; the kernel executes it in a cage (64 MB heap / 1 MB stack / hard deadline) and hands back one JSON result. Capabilities — `host.fs`, `host.llm` — are APIs *inside* the program. Control flow (loops, parsing, retries, sub-agent delegation) lives inside the program too, so the agent framework needs no harness.

[中文文档](README.zh-CN.md)

## Quick start

```sh
cargo build --release
echo 'return 1+1' | ./target/release/terrarium
```

```json
{
  "ok": true,
  "result": "2",
  "stdout": "",
  "error": null,
  "timed_out": false,
  "elapsed_ms": 1,
  "target": "x86_64-linux",
  "limits": { "memory": "64MB", "stack": "1MB", "timeout_ms": 2000 },
  "mounts": [],
  "llm_usage": { "calls": 0, "cache_hit_tokens": 0, "cache_miss_tokens": 0, "output_tokens": 0 }
}
```

Mount a project and print the full agent contract (the exact text your LLM is taught):

```sh
./target/release/terrarium --mount /proj=$(pwd) --contract
```

## Command line

Run mode — execute one program (code from argv, or stdin when omitted):

```sh
terrarium [--timeout N] [--mount /virt=real[:rw]]... [--contract] [code]
```

Agent mode — an outer loop that talks to an LLM; conversation state lives outside the cage, every run gets a fresh one:

```sh
terrarium agent <task-file | task text> [--mount /virt=real[:rw]]... [--max-rounds N] [--run-timeout-ms N]
```

## Host API

Inside a program the model gets `host` (run `host.help()` for the live surface, or `--contract` for the full contract):

- `host.fs.{list,read,text,search,write}` — zooming exploration: `list` (sizes come free) → windowed `read`; full-repo `search` runs host-side so file contents never enter the model's context; `text` reads a whole file into the program for spot edits; `write` is text-only, atomic, auto-creates parent dirs.
- `host.llm.{call,chat}` — nested LLM turns.
- Writing is allowed only where the operator declared a `:rw` mount at startup. The kernel enforces physics, not judgment; a policy denial belongs in the model's final answer, not in a retry.

Built into every program: `runBlock(code)` (nested run, same semantics) and `spawnAgent(task, {system?, maxTurns=8})` (fresh-context sub-agent). Sub-agents are just main agents with a different context — one contract teaches both.

## Sandbox limits

Each run gets 64 MB heap, 1 MB stack, and one hard deadline (`--timeout`, default 2000 ms). A dead run (OOM, timeout) kills that run only — the session survives; the next run starts clean.

## Configuration

| Variable | Purpose |
|---|---|
| `DEEPSEEK_API_KEY` | Required for `host.llm` and agent mode |
| `TERRARIUM_LLM_BASE_URL` | Endpoint override (any OpenAI-compatible provider) |
| `TERRARIUM_LLM_MODEL` | Model override |

Keys live only in the process environment — the sandbox can't see them. In agent mode, if the environment doesn't carry the key, terrarium also falls back to reading it from a `./.env` file.

## Repository layout

- `src/main.rs` — kernel pipeline: fresh cage per run, limits, cancellation protocol
- `src/registry.rs` — host API registry; `host.help()` and the contract are generated from it, so they cannot drift
- `src/fs.rs`, `src/llm.rs` — host capabilities (mounts, LLM endpoint)
- `src/agent.rs` — the outer agent loop
- `src/CONTRACT.md`, `src/MAIN.md`, `src/prelude.js` — teaching contract, role template, runtime helpers; compiled into the binary via `include_str!`

## License

[MIT](LICENSE)
