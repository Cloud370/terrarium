# terrarium

An agent runtime where the model's actions are programs, not tool calls. Each turn submits one complete ES2020 JavaScript program; the kernel runs it in a fresh QuickJS cage with a 64 MB heap, 1 MB stack, and hard deadline, then returns one structured JSON result.

[中文文档](README.zh-CN.md)

## Core direction

Terrarium is a bounded program runtime: the model emits programs, the host exposes explicit capabilities, each run executes in a fresh cage. It is not a tool registry, a shell wrapper, an operating-system sandbox, or a multi-agent framework.

Invariants:

- Model actions are programs — one complete `run` block per turn, and nothing outside that block executes.
- Capabilities stay explicit, minimal, typed, bounded, and observable; errors surface at the boundary instead of falling back silently.
- Security lives in the host — mount scoping, `:rw` writes, resource limits, cancellation. Prompts describe behavior; they never provide the boundary.
- No mutable state crosses runs, and credentials never enter the cage.
- Core behavior is host code with no platform-specific external commands.
- `host.fs.scan` is the only search engine: host-side pruning plus ordinary JavaScript filtering.
- A stateless `host.llm.call` is not an agent session. A sub-agent becomes a capability only with its own lifecycle, budgets, cancellation, narrowed mounts, and structured result.

Before adding any capability, answer: which real workflow needs it; what are its limits, cancellation, failure states, and permissions; does it work the same on Linux, macOS, and Windows; and can an existing capability express it? Prefer the smallest boundary, and keep speculative features out of the public contract.

## The protocol

One agent session is a sequence of programs. A model reply must contain one closed `run` fence:

````text
```run
for await (const line of host.fs.scan("/proj/src", {glob: "*.rs"})) {
  if (line.text.includes("http client")) return `${line.file}:${line.no}`;
}
```
````

A normal `return` ends only that run. The session ends when the program calls `host.agent.answer(text)`:

````text
```run
host.agent.answer("The HTTP client is configured in src/llm.rs.");
```
````

The parser accepts exactly one complete `run` fence per reply. A missing fence, an unclosed fence, or more than one `run` fence is a protocol error — the parser never executes one block and silently ignores the rest. An opening fence is a line that reads exactly ```` ```run ```` and a closing fence is a standalone ```` ``` ```` line; inline triple backticks never open or close a block. Text outside the block is not executed. There is no text-based completion marker.

Every run is evaluated as one async function body, so top-level `return` and `await` have the same meaning in every program. The result keeps JSON values as JSON instead of formatting them as strings.

## Why programs

- A whole unit of work executes per turn; context is spent on findings instead of tool-call bookkeeping.
- JavaScript supplies control flow, retries, branching, and concurrency through ordinary language constructs.
- The host surface stays small: filesystem capabilities, nested text-only LLM calls, and the explicit session answer function.
- Each run has a fresh cage, so a failed run does not corrupt the next run.

## The cage

- Per run: 64 MB heap, 1 MB stack, and one hard deadline. Agent mode defaults to 10 seconds; single-run mode defaults to 2 seconds. A first-line `// timeout-ms: N` directive may raise an agent run up to 300 seconds.
- Captured stdout is limited to 16 KB. Host file reads use bounded windows or a bounded whole-file channel.
- Filesystem access exists only under operator-declared mounts. `:rw` is required for writes. Path escapes and symlinks that resolve outside a mount are rejected; scans never follow symlinks.
- API credentials remain in the host process environment and are never exposed to JavaScript.

## Quick start

```sh
cargo build --release
echo 'return 1+1' | ./target/release/terrarium
```

The command prints one JSON object:

```json
{
  "ok": true,
  "value": 2,
  "answer": null,
  "stdout": "",
  "error": null,
  "termination": "returned",
  "timed_out": false,
  "elapsed_ms": 1,
  "target": "x86_64-linux",
  "limits": { "memory": "64MB", "stack": "1MB", "timeout_ms": 2000 },
  "mounts": [],
  "llm_usage": { "calls": 0, "cache_hit_tokens": 0, "cache_miss_tokens": 0, "output_tokens": 0 }
}
```

Print the exact contract used for a mounted project:

```sh
./target/release/terrarium --mount /proj=$(pwd) --contract
```

## Command line

Run one program from arguments, or from stdin when no code argument is supplied:

```sh
terrarium [--timeout-ms N] [--mount /virt=real[:rw]]... [--contract] [code]
```

Run the outer agent loop:

```sh
terrarium agent <task-file | task text> [--mount /virt=real[:rw]]... [--max-rounds N] [--run-timeout-ms N]
```

All time values are milliseconds. Run mode exits `0` for a successful program, `1` for a failed run, and `2` for usage/configuration errors. Agent mode exits `0` after `host.agent.answer`, `1` for transport or usage failure, and `2` when the round budget is exhausted.

## Host API

The generated contract (`--contract`) documents the live surface:

- `host.fs.list(dir)` lists one directory level, including sizes and symlink entries.
- `host.fs.read(path, from, to)` reads a bounded line window. `to=Infinity` reads to EOF within the window budget.
- `host.fs.text(path)` reads a whole text file into the program when it fits the 64 MB host budget.
- `host.fs.scan(path, options)` streams text-file lines from a directory tree. It respects `.gitignore`, skips hidden entries, binaries, and symlinks by default, and validates option types. Traversal and decoding errors reject the scan rather than becoming an empty result.
- `host.fs.write(path, content)` atomically writes text under a declared `:rw` mount and returns the byte count.
- `host.llm.call(prompt, system)` makes a nested text request through the configured OpenAI-compatible chat-completions endpoint. The call is stateless: the nested model sees only the supplied prompt and system text, with no contract, mounts, or host capabilities. There is no nested multi-turn chat; that belongs to a future sub-agent session.
- `host.agent.answer(text)` commits the current agent session answer. Returning from a program never commits the session.

The current model examples declare these capabilities:

- `deepseek-v4-flash`: text input and text output; it does not accept image input.
- `deepseek-v4-flash-vision-exp`: text or image input and text output.

This phase only declares those capabilities. The implemented `host.llm` request payload is text-only; image file reading, encoding, and artifact transport are not implemented.

## Configuration

| Variable | Purpose |
|---|---|
| `TERRARIUM_LLM_API_KEY` | API key for agent mode and `host.llm` |
| `TERRARIUM_LLM_BASE_URL` | OpenAI-compatible chat-completions endpoint |
| `TERRARIUM_LLM_MODEL` | Model ID sent upstream; defaults to `deepseek-v4-flash` |
| `TERRARIUM_LOG_RUNS` | Set to `1` to log executed run source to stderr |

The binary does not load `.env` files. Supply credentials through the process environment or an external secret manager. Keep secret files outside mounted directories.

## Repository layout

- `src/lib.rs`, `src/kernel.rs` — reusable kernel boundary and one fresh cage per run
- `src/main.rs`, `src/cli.rs` — process and terminal adapters
- `src/agent.rs` — outer agent loop and run-fence parser
- `src/fs.rs`, `src/llm.rs`, `src/registry.rs` — host capabilities and live API registry
- `src/prompts/`, `src/runtime/` — embedded model prompt and JavaScript runtime assets
- `docs/` — maintained design, protocol, configuration, security, and integration notes

The library exposes `Kernel` and validated `Mount` for non-CLI callers. A future Web UI should add a service adapter over this library instead of spawning the binary or scraping stderr.

## Documentation

- [Design direction](docs/design.md)
- [Current protocol](docs/protocol.md)
- [Configuration](docs/configuration.md)
- [Security boundary](docs/security.md)
- [Web UI integration boundary](docs/web-ui.md)

## License

[MIT](LICENSE)
