# Protocol

This document describes the protocol implemented by the current binary and library. The default `terrarium` command is a durable model-driven agent; `terrarium run` is the only direct JavaScript entry point.

## Agent replies

The outer agent loop accepts one complete fenced program per model step:

````text
```run
const matches = [];
for await (const line of host.fs.scan("/proj/src", {glob: "*.rs"})) {
  if (line.text.includes("http client")) matches.push({file: line.file, line: line.no});
}
return {to: "model", facts: {matches}};
```
````

A reply must contain exactly one closed `run` block. A missing block, an unclosed block, or more than one block is a protocol error; the parser never executes one block and silently ignores the rest. An opening fence is a line that reads exactly ```` ```run ```` and a closing fence is a standalone ```` ``` ```` line. Inline triple backticks never open or close a block, and text outside the block is not executed.

In agent mode, a successful program must return exactly one tagged disposition:

- `{to: "model", facts: {...}}` ends the current JavaScript run and continues the same user turn. `facts` is a bounded object that serializes to at most 16384 bytes for the next model step.
- `{to: "user", message: "..."}` ends the current turn and prints the message to the user. Use this only when the result is established or a specific user action, missing input, authorization, or decision is required.

A normal top-level `return` releases the run's local JavaScript state; it does not by itself finish a turn. A format, parse, traversal, validation, timeout, or other recoverable operation error is model feedback, not an automatic user handoff. The next step should correct the operation, narrow the scope, or gather the missing evidence. A `catch` block that merely reports an error must return short facts to `to: "model"`, not hand control to the user. Only a real need for user input, authorization, or a decision belongs in `to: "user"`.

## Context budget

A run has two data channels. Program-provided data enters the next model context only through `to: "model"` `facts`; local variables, `print` output, and a `to: "user"` message do not become next-step model facts. The host may add bounded status, errors, and write receipts as trusted evidence. Keep `facts` to decision-relevant paths, counts, statuses, and bounded samples. Do not return complete scan results, whole file contents, or large arrays. If a large result must survive the run, write it to an authorized file and return only its path, count, and short summary. The 24 KiB result limit and 16 KiB facts limit are hard boundaries, not targets.

## Run semantics

Every program is wrapped and evaluated as one async function body. Top-level `return` and `await` are therefore legal in every run. The kernel does not infer execution mode from source shape and does not use an implicit last-expression result.

Each run uses a fresh QuickJS runtime with a 64 MiB heap, 1 MiB stack, bounded stdout, bounded host reads, and a validated deadline. Filesystem capabilities are available only under mounts supplied by the operator. The agent invocation installs `/workspace` for the working root, or `/` for `--full-access`, plus any explicit `--mount` entries; that mount set is reused for every run in the invocation.

## Run result

Single-run mode writes one JSON object. The reusable library returns the same fields as an `Outcome`:

```json
{
  "ok": true,
  "value": {"answer": 42},
  "stdout": "",
  "error": null,
  "termination": "returned",
  "timed_out": false,
  "elapsed_ms": 6,
  "writes": [],
  "writes_truncated": false
}
```

- `value` is an optional JSON-compatible value returned by the program. `undefined` becomes `null` in the CLI JSON object.
- `error` is either `null` or `{kind, message}`.
- `termination` is `returned`, `failed`, `timed_out`, `cancelled`, or `fatal`.
- `timed_out` is a compatibility convenience and is true only for a timed-out run.
- `writes` contains at most 64 host-derived receipts for committed writes. Each receipt has `path`, `created`, `changed`, `bytesBefore`, `bytesAfter`, and `firstChangedLine`; `firstChangedLine` is `null` for a byte-identical rewrite. `writes_truncated` reports omitted receipts.

The direct `terrarium run` command accepts any JSON-compatible return value. Agent mode validates the returned value at the host boundary as one of the two tagged dispositions and records the normalized disposition in `run/result`.

## Host error behavior

Host calls reject with a useful error instead of silently producing an empty result. In particular, scan traversal, file opening, and UTF-8 decoding failures carry the virtual path. Scan option fields reject when their types are wrong. Hidden entries, symlinks, binary files, and `.gitignore` matches are intentional scan exclusions, not errors. `walk` shares scan's traversal engine and option set; it yields `{file, size}` entries and reports traversal failures only, since it never opens files.

A failed JavaScript run produces a compact model observation containing turn and step coordinates, status, termination, timeout, elapsed time, a bounded error classification, and any write receipts committed before the failure. It does not automatically copy the returned value or stdout into model context. A successful `to: "model"` disposition produces an observation containing the same coordinates, its bounded facts, and any write receipts. A successful `to: "user"` disposition produces no model observation; the outer loop appends `turn/end` with `reason: "handed_off"` and prints the message.

## LLM payload scope

The main model request is owned by the trusted outer agent loop. Each request uses the current turn's frozen resolved profile and is persisted as `model/request` before one network dispatch; a retryable attempt-1 failure may create exactly one attempt 2. Failed model results are not projected into later model-visible history.

JavaScript has no `host.llm.call` or other in-program model-call primitive. The host surface is filesystem capabilities plus the tagged return protocol for continuing or handing off a turn. The configured model ID is sent unchanged to the OpenAI-compatible endpoint.

The built-in capability declaration says `deepseek-v4-flash` accepts text input only, while `deepseek-v4-flash-vision-exp` declares text and image input. The latter declaration does not enable image payloads yet.

TOML provider/profile configuration and durable JSONL sessions are implemented. The session stores no credential value or access mode; `--read-only`, `workspace`, and `--full-access` are selected by the current invocation.
