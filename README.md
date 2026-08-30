# terrarium

An agent runtime where the model's actions are programs, not tool calls. The default command runs a durable model-driven agent session; `terrarium run` is the explicit direct-JavaScript entry point. Each JavaScript run executes in a fresh QuickJS cage with a 64 MB heap, 1 MB stack, and hard deadline, then returns a structured result.

[中文文档](README.zh-CN.md)

## Core direction

Terrarium is a bounded program runtime: the model emits programs, the host exposes explicit capabilities, each run executes in a fresh cage. It is not a tool registry, a shell wrapper, an operating-system sandbox, or a multi-agent framework.

### Reasoning discipline

Every new capability starts with first principles, not with an API shape or an implementation pattern. Before adding it, answer these questions in order:

1. What user outcome does this enable, and what is the smallest workflow that proves the outcome?
2. Which facts and effects must cross the current boundary, and which are only temporary computation?
3. Who owns each state, who may change it, and when does it begin and end?
4. What is the smallest explicit interface that makes that ownership and lifecycle visible?
5. What happens on failure, timeout, cancellation, process loss, restart, partial completion, and denied permission?
6. Which data belongs in model context, which belongs in durable state, and which must remain outside both?
7. Can the existing boundaries express the workflow? If so, prefer composing them over adding a new abstraction.

Keep control flow separate from data flow: a result should say who acts next, while large or sensitive data should cross boundaries by an explicit, bounded reference. Facts owned by the host must be derived by the host, not reported by the model. Optimize for the fewest steps that establish correctness, never for spending or exposing a step budget. Do not introduce a lifecycle, storage layer, routing mechanism, or capability without a concrete consumer and a complete contract for its limits and recovery, and keep speculative features out of the public contract.

The design is good when its behavior can be reconstructed from its boundaries: a user, a model, or a future maintainer should be able to tell what persists, what is released, who acts next, and how uncertainty is handled without reading hidden implementation details.

Invariants:

- Model actions are programs — exactly one complete `run` fence per model response, and nothing outside that fence executes.
- Each run executes in a fresh cage; no mutable state crosses runs, and credentials never enter the cage.
- Security lives in the host — mount scoping, `:rw` writes, resource limits, cancellation. Prompts describe behavior; they never provide the boundary.
- Capabilities stay explicit, minimal, typed, bounded, and observable; errors surface at the boundary instead of falling back silently.
- Search stays composed: the host prunes (gitignore, glob, literal `contains`) and JavaScript applies the final predicate — there is deliberately no host grep or regex capability.
- Sessions are durable append-only JSONL files. Model requests and runs are journaled before dispatch; a run whose outcome is unknown is marked, never replayed.
- Core behavior is portable host code with no platform-specific external commands, so Linux, macOS, and Windows behave the same.

## The protocol

One agent turn is a sequence of steps. Each model response must contain one closed `run` fence, and each successful program must return one explicit disposition:

````text
```run
const matches = [];
for await (const line of host.fs.scan("/proj/src", {glob: "*.rs"})) {
  if (line.text.includes("http client")) matches.push({file: line.file, line: line.no});
}
return {to: "model", facts: {matches}};
```
````

`to: "model"` ends the current JavaScript run and continues the same user turn. `to: "user"` ends the current turn and prints its message:

````text
```run
return {to: "user", message: "The HTTP client is configured in src/llm/."};
```
````

A normal `return` releases the run's local JavaScript state; it does not by itself finish a turn. A returned error is not automatically a user-facing result: format, parse, traversal, validation, timeout, and other recoverable failures should return short facts to `to: "model"` so the next step can correct the work. Use `to: "user"` only when the result is established or a specific user action, missing input, authorization, or decision is required. A `catch` block that merely reports an error must not end the turn.

The parser accepts exactly one complete `run` fence per reply. A missing, unclosed, or duplicated fence is a protocol error — the parser never runs the first block and silently ignores the rest. The opening fence is a standalone ```` ```run ```` line, the closing fence a standalone ```` ``` ```` line; inline triple backticks neither open nor close a block. Text outside the fence does not execute, and there is no text-based completion marker.

Every run executes as the same kind of async function body, so top-level `return` and `await` are legal in every program. A returned value keeps its JSON structure instead of being flattened to a string.

## Context budget

A run has two data channels. Program-provided data enters the next model context only through `to: "model"` `facts`; local variables, `print` output, and a `to: "user"` message do not become next-step model facts. The host may add bounded status, errors, and write receipts as trusted evidence. Keep `facts` to decision-relevant paths, counts, statuses, and bounded samples. Do not return complete scan results, whole file contents, or large arrays. If a large result must survive the run, write it to an authorized file and return only its path, count, and short summary. The 24 KiB result limit and 16 KiB facts limit are hard boundaries, not targets.

## Why programs

- A whole unit of work executes per step; context is spent on findings instead of tool-call bookkeeping.
- JavaScript supplies control flow, retries, branching, and concurrency through ordinary language constructs.
- The host surface stays small: bounded filesystem capabilities and explicit model/user dispositions. The main model is called by the trusted outer loop; JavaScript has no model-call primitive.
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

The normal command always starts or resumes the model-driven agent. Message arguments are joined as text; non-terminal stdin supplies a message when no message argument is present. `--mount` entries apply to every run in the invocation. `workspace` is the default access mode, `--read-only` and `--full-access` are mutually exclusive, and access mode and mounts are never stored in the session. The agent exits `0` after a program returns `to: "user"`, and `2` for usage or configuration errors. Direct-run exits `0` for a successful program and `1` for a failed program.

## Host API

The generated contract (`--contract`) documents the live surface:

- `host.fs.list(dir)` lists one directory level as sorted objects with `name`, `type` (`file`, `directory`, `symlink`, or `other`), and `size` in bytes for regular files (`null` otherwise).
- `host.fs.read(path, from, to)` reads a bounded line window and returns stable `N: text` line numbers plus a continuation footer.
- `host.fs.text(path)` reads a whole text file into the program as LF-normalized text without display line numbers. Use it for program-side transformations, not for displaying code.
- `host.fs.replace(path, oldText, newText[, {all}])` performs one exact targeted replacement. It requires one match by default, fails loudly for missing or ambiguous text, treats replacement text literally, and uses `{all: true}` only for intentional all-match replacement. When the old text is already known, this is the efficient one-call edit path; when it is not known, read or scan first for enough context. Do not re-read solely to confirm a write; the run result includes the host-derived receipt.
- `host.fs.scan(path, options)` streams text-file lines from a directory tree. Pass optional `contains: "literal"` to let Rust discard non-matching lines before they cross into JavaScript; JavaScript remains the final predicate for regexes, case rules, multiple conditions, cross-line state, and custom limits. Without it, every line is yielded as before. It respects `.gitignore`, skips hidden entries, binaries, and symlinks by default, and validates option types. Traversal and decoding errors reject the scan rather than becoming an empty result.
- `host.fs.walk(path, options)` streams one `{file, size}` per regular file from a directory tree — the file-level twin of `scan`, with the same pruning and the same options; files are never opened. Counting files or summing sizes is a walk; counting `scan` yields counts lines.
- `host.fs.write(path, content)` atomically writes text under a declared `:rw` mount and returns the byte count. The run result also includes bounded host-derived write receipts (`path`, `created`, `changed`, `bytesBefore`, `bytesAfter`, `firstChangedLine`).

Agent programs use the tagged return protocol described above for model continuation or user handoff.

Model requests belong to the trusted outer agent loop and are journaled in the session; the JavaScript host surface is the filesystem capability set above.

The contract opens with the configured model's declared capabilities (text-only versus text-and-image input; a model without a local declaration is labeled undeclared). The request payload stays text-only regardless — image file reading, encoding, and artifact transport are not implemented.

## Configuration

The preferred configuration is a strict TOML file at `$XDG_CONFIG_HOME/terrarium/config.toml`, or `~/.config/terrarium/config.toml` on Unix when `XDG_CONFIG_HOME` is unset. Pass another file with `--config PATH`. Credentials are referenced by environment-variable name and are never stored in the session.

Profiles select one of three wire protocols — `openai-chat-completions`, `openai-responses`, or `anthropic-messages` (DeepSeek's Anthropic-compatible endpoint works via `base_url = "https://api.deepseek.com/anthropic"`). Every call streams over server-sent events under a per-attempt total timeout and an inter-chunk idle timeout, both configurable per profile. Assistant reasoning is journaled with each result and replayed on later requests in the protocol's own shape (assistant `reasoning_content` for Chat Completions, encrypted reasoning items for Responses, signed thinking blocks for Anthropic). Per-request token usage — net input, output, cache read/write — is journaled and reported as a context-budget line against the profile's declared `context_window`.

If no TOML file is selected, the legacy `TERRARIUM_LLM_API_KEY`, `TERRARIUM_LLM_BASE_URL`, and `TERRARIUM_LLM_MODEL` variables remain supported as a compatibility fallback. The binary does not load `.env` files.

## Repository layout

- `src/lib.rs`, `src/kernel.rs` — reusable kernel boundary and one fresh cage per run
- `src/main.rs`, `src/cli.rs` — process and terminal adapters
- `src/agent.rs` — outer agent loop and run-fence parser
- `src/session.rs` — durable append-only session journal
- `src/fs.rs`, `src/llm/`, `src/registry.rs` — host capabilities, streaming three-protocol model transport, and the live API registry
- `src/prompts/`, `src/runtime/` — embedded model prompt and JavaScript runtime assets
- `docs/` — maintained design, protocol, configuration, security, and integration notes

The library exposes `Kernel` and validated `Mount` for non-CLI callers. A future Web UI should add a service adapter over this library instead of spawning the binary or scraping stderr.

## Documentation

- [Design direction](docs/design.md)
- [Current protocol](docs/protocol.md)
- [Configuration](docs/configuration.md)
- [Security boundary](docs/security.md)
- [Model profiles and durable sessions](docs/model-profiles-and-durable-sessions.md)
- [Web UI integration boundary](docs/web-ui.md)

## License

[MIT](LICENSE)
