# terrarium

An agent runtime where the model's actions are programs, not tool calls. The default command runs a durable model-driven agent session; `terrarium run` is the explicit direct-JavaScript entry point. Each JavaScript run executes in a fresh QuickJS cage with a 64 MB heap, 1 MB stack, and hard deadline, then returns a structured result.

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
- Sessions are durable append-only JSONL files. Model requests and JavaScript runs cross a durable boundary before dispatch; uncertain runs are never replayed.

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
- The host surface stays small: bounded filesystem capabilities and the explicit session answer function. The main model is called by the trusted outer loop; JavaScript has no model-call primitive.
- Each run has a fresh cage, so a failed run does not corrupt the next run.

## The cage

- Per run: 64 MB heap, 1 MB stack, and one hard deadline. Agent mode defaults to 10 seconds; single-run mode defaults to 2 seconds. A first-line `// timeout-ms: N` directive may raise an agent run up to 300 seconds.
- Captured stdout is limited to 16 KB. Host file reads use bounded windows or a bounded whole-file channel.
- Filesystem access exists only under operator-declared mounts. `:rw` is required for writes. Path escapes and symlinks that resolve outside a mount are rejected; scans never follow symlinks.
- API credentials remain in the host process environment and are never exposed to JavaScript.

## Quick start

Configure one or more model profiles in `config.toml`:

```toml
version = 1
default_profile = "main"

[providers.local]
base_url = "http://127.0.0.1:11434/v1"

[profiles.main]
provider = "local"
protocol = "openai-chat-completions"
model = "qwen3-coder"
```

Start the model-driven agent in the current directory:

```sh
terrarium "review this project"
terrarium --profile main --read-only "find the unused dependencies"
```

To let one invocation use real absolute paths outside the working directory, select full access:

```sh
terrarium --full-access "read ~/chat/landscape-monitor"
```

For a narrower authorization that remains available to every run in the invocation, add an explicit mount:

```sh
terrarium --read-only \
  --mount /landscape-monitor="$HOME/chat/landscape-monitor" \
  "read landscape-monitor"
```

`--full-access` maps `/` to the current user's filesystem view; it does not bypass operating-system permissions. In restricted modes, the agent uses `/workspace` plus any explicit virtual mounts. JavaScript does not expand `~`; the prompt identifies the available roots and tells the model how to handle a denied path.

For direct JavaScript execution, use the separate `run` command:

```sh
terrarium run -e 'return 1 + 1'
```

The agent stores its session under the per-user state directory and prints the session ID to stderr when creating a session. Direct runs create no session.

## Command line

```sh
terrarium [--config PATH] [--profile NAME] [--read-only | --full-access] [--mount /virtual=real[:rw]] [--max-steps N] [--run-timeout-ms N] [message...]
terrarium --resume SESSION_ID [--read-only | --full-access] [--mount /virtual=real[:rw]] [message...]
terrarium run [-e SOURCE | FILE] [--read-only | --full-access] [--mount /virtual=real[:rw]] [--timeout-ms N]
```

The normal command always starts or resumes the model-driven agent. Message arguments are joined as text; non-terminal stdin supplies a message when no message argument is present. `--mount` entries apply to every run in the invocation. `workspace` is the default access mode, `--read-only` and `--full-access` are mutually exclusive, and access mode and mounts are never stored in the session. The agent exits `0` after `host.agent.answer`, and `2` for usage or configuration errors. Direct-run exits `0` for a successful program and `1` for a failed program.

## Host API

The generated contract (`--contract`) documents the live surface:

- `host.fs.list(dir)` lists one directory level as sorted objects with `name`, `type` (`file`, `directory`, `symlink`, or `other`), and `size` in bytes for regular files (`null` otherwise).
- `host.fs.read(path, from, to)` reads a bounded line window. `to=Infinity` reads to EOF within the window budget.
- `host.fs.text(path)` reads a whole text file into the program when it fits the 64 MB host budget.
- `host.fs.scan(path, options)` streams text-file lines from a directory tree. It respects `.gitignore`, skips hidden entries, binaries, and symlinks by default, and validates option types. Traversal and decoding errors reject the scan rather than becoming an empty result.
- `host.fs.write(path, content)` atomically writes text under a declared `:rw` mount and returns the byte count.
- `host.agent.answer(text)` commits the current agent session answer. Returning from a program never commits the session.

The JavaScript host surface does not include `host.llm.call`; model requests belong to the trusted outer agent loop and are recorded in the session journal.

The current model examples declare these capabilities:

- `deepseek-v4-flash`: text input and text output; it does not accept image input.
- `deepseek-v4-flash-vision-exp`: text or image input and text output.

This phase only declares these model capabilities. The outer model request payload is text-only; image file reading, encoding, and artifact transport are not implemented.

## Configuration

The preferred configuration is a strict TOML file at `$XDG_CONFIG_HOME/terrarium/config.toml`, or `~/.config/terrarium/config.toml` on Unix when `XDG_CONFIG_HOME` is unset. Pass another file with `--config PATH`. Credentials are referenced by environment-variable name and are never stored in the session.

If no TOML file is selected, the legacy `TERRARIUM_LLM_API_KEY`, `TERRARIUM_LLM_BASE_URL`, and `TERRARIUM_LLM_MODEL` variables remain supported as a compatibility fallback. The binary does not load `.env` files.

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
