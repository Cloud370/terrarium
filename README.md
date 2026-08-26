# terrarium

An agent runtime where **the model's actions are programs, not tool calls.** Each turn the LLM writes one complete ES2020 JavaScript program; the kernel executes it in a fresh sandbox cage (64 MB heap / 1 MB stack / hard deadline) and hands back one JSON result. Loops, retries, branching, parallelism, sub-agent delegation — everything an agent harness usually implements — are language constructs *inside* the program. The harness is not a tool dispatcher; it is a kernel: execute, enforce limits, return JSON.

[中文文档](README.zh-CN.md)

## The protocol

One session is: task → programs → final answer. Every model reply is exactly one of two things:

- a fenced `run` block — one complete program; the kernel executes it and feeds the JSON result back as the next message
- a `FINAL:` line — the committed answer; nothing after it is extracted or run

````
task:   Where does the kernel set its HTTP timeout?

reply:  ```run
        return host.fs.search("http client", 20).filter(h => h.startsWith("/proj/src/"))
        ```
        (the repo is scanned host-side — file contents never enter the model's context)

result: {"round":3,"ok":true,"result":"[\"/proj/src/main.rs:262: …\"]","stdout":"","error":null,"timed_out":false,"elapsed_ms":6}

reply:  FINAL: src/main.rs:262 — the HTTP client is built there with a 120 s per-request timeout.
````

Why a `run` fence, not ` ```javascript `? A language tag describes content, and models write language-tagged fences all the time to *show* code. `run` is an instruction to the runtime with exactly one meaning: execute this program. (The kernel still tolerates ` ```js `/` ```javascript ` fences and `<run>` tags as a robustness fallback, but the contract teaches a single marker. And the dialect is not plain JavaScript anyway: top-level `return` and `await`, `host.*` APIs, `runBlock`/`spawnAgent` built-ins.)

## Why programs, not tool calls

- One round-trip, one unit of work: a whole plan executes per turn instead of one tool call — context is spent on results, not on turn-taking.
- Control flow is free: retry is `try/catch`, branching is `if`, parallelism is `Promise.all`, delegation is `spawnAgent(task)` — no harness feature has to exist before the model can use it.
- The capability surface stays tiny because the language is the combinator: `host.fs` and `host.llm` are all there is.
- Failures are cheap: each run gets a fresh cage; a dead run (OOM, timeout) kills that run only — the session survives and the next run starts clean.

## The cage

- Per run: 64 MB heap, 1 MB stack, one hard deadline. Defaults to 2 s; agent mode caps it at 300 s, and a program can request a larger budget with a `// timeout-ms: N` first line.
- Filesystem access only through mounts declared at launch (`--mount /virt=real`; writable only with `:rw`). Escapes (`..`, symlinks out of the root) are rejected by path physics, not judgment — a policy denial belongs in the final answer, not in a retry.
- API keys live in the host process environment; the sandbox never sees them.

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

Built into every program: `runBlock(code)` (nested run, same semantics) and `spawnAgent(task, {system?, maxTurns=8})` (fresh-context sub-agent — a main agent with a different context; one contract teaches both).

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
