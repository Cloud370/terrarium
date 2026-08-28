# Protocol

This document describes the protocol implemented by the current binary and library. The default `terrarium` command is a durable model-driven agent; `terrarium run` is the only direct JavaScript entry point.

## Agent replies

The outer agent loop accepts one complete fenced program per model turn:

````text
```run
const files = [];
for await (const line of host.fs.scan("/proj/src", {glob: "*.rs"})) {
  if (line.text.includes("http client")) files.push(`${line.file}:${line.no}`);
}
return files;
```
````

A reply must contain exactly one closed `run` block. A missing block, an unclosed block, or more than one block is a protocol error; the parser never executes one block and silently ignores the rest. An opening fence is a line that reads exactly ```` ```run ```` and a closing fence is a standalone ```` ``` ```` line — inline triple backticks never open or close a block. There are no compatibility fence types or text completion markers.

Returning from a program submits an observation for the next turn. To finish the whole agent session, the program must call:

```js
host.agent.answer("The decisive finding is ...");
```

The call records the supplied text as the session answer. A normal `return` never does this, even when its value looks like a final report.

## Run semantics

Every program is wrapped and evaluated as one async function body. Top-level `return` and `await` are therefore legal in every run. The kernel does not infer execution mode from source shape and does not use an implicit last-expression result.

Each run uses a fresh QuickJS runtime with a 64 MiB heap, 1 MiB stack, bounded stdout, bounded host reads, and a validated deadline. Filesystem capabilities are available only under mounts supplied by the operator. The agent invocation installs `/workspace` for the working root, or `/` for `--full-access`, plus any explicit `--mount` entries; that mount set is reused for every run in the invocation.

## Run result

Single-run mode writes one JSON object. The reusable library returns the same fields as an `Outcome`:

```json
{
  "ok": true,
  "value": {"answer": 42},
  "answer": null,
  "stdout": "",
  "error": null,
  "termination": "returned",
  "timed_out": false,
  "elapsed_ms": 6
}
```

- `value` is an optional JSON-compatible value returned by the program. `undefined` becomes `null` in the CLI JSON object.
- `answer` is non-null only after `host.agent.answer(text)` was called.
- `error` is either `null` or `{kind, message}`. Kinds are `runtime`, `capability`, `configuration`, `internal`, and `protocol`.
- `termination` is `returned`, `failed`, `timed_out`, `cancelled`, or `fatal`.
- `timed_out` is a compatibility convenience and is true only for a timed-out run.

The CLI adds target, limits, mount, and process-lifetime LLM usage metadata. These are adapter fields, not additional kernel protocol requirements.

## Host error behavior

Host calls reject with a useful error instead of silently producing an empty result. In particular, scan traversal, file opening, and UTF-8 decoding failures carry the virtual path. Scan option fields reject when their types are wrong. Hidden entries, symlinks, binary files, and `.gitignore` matches are intentional scan exclusions, not errors.

## LLM payload scope

The main model request is owned by the trusted outer agent loop. Each request uses the current turn's frozen resolved profile and is persisted as `model/request` before one network dispatch; a retryable attempt-1 failure may create exactly one attempt 2. Failed model results are not projected into later model-visible history.

JavaScript has no `host.llm.call` or other in-program model-call primitive. The host surface is filesystem capabilities plus `host.agent.answer(text)` for session completion. The configured model ID is sent unchanged to the OpenAI-compatible endpoint.

The built-in capability declaration says `deepseek-v4-flash` accepts text input only, while `deepseek-v4-flash-vision-exp` declares text and image input. The latter declaration does not enable image payloads yet.

TOML provider/profile configuration and durable JSONL sessions are implemented. The session stores no credential value or access mode; `--read-only`, `workspace`, and `--full-access` are selected by the current invocation.
