# Protocol

This document describes the protocol implemented by the current binary and library. The default `terrarium` command is a durable model-driven agent; `terrarium run` is the only direct JavaScript entry point.

## Agent replies

The outer agent loop accepts one complete response per model step: an optional `access` block followed by exactly one closed `run` block.

````text
```access
{"writes": ["/home/me/proj/notes.md"], "reason": "append the scan summary to the project notes"}
```
```run
const matches = [];
for await (const line of host.fs.scan("/home/me/proj/src", {glob: "*.rs"})) {
  if (line.text.includes("http client")) matches.push({file: line.file, line: line.no});
}
return {to: "model", facts: {matches}};
```
````

A reply must contain exactly one closed `run` block. A missing block, an unclosed block, more than one `run` block, more than one `access` block, an `access` block appearing after the `run` block, or an `access` block whose body is not valid access JSON is a protocol error; the parser never executes one block and silently ignores the rest. An opening fence is a line that reads exactly ```` ```run ```` or ```` ```access ````, a closing fence is a standalone ```` ``` ```` line. Inline triple backticks never open or close a block, and text outside the blocks is not executed.

### The access block

The `access` block is the write-preauthorization request. Its body is one strict JSON object with exactly two fields:

```json
{"writes": ["/abs/path/file.md"], "reason": "why this run must write those files"}
```

- `writes` is an array of at most 32 absolute file paths. A target must be an exact file path: relative paths, `.`, `..`, empty or ambiguous `//` segments, trailing slashes, glob characters, directories, existing symbolic-link targets, and paths that duplicate an earlier entry after normalization are all rejected. Missing files are valid targets — approving one includes creating its missing parent directories.
- `reason` is at most 200 characters. A non-empty `writes` array requires a non-empty `reason` in `planned-write` mode.
- The whole encoded block is at most 4 KiB.

A run that will not write sends an empty request (or omits the block entirely):

```json
{"writes": [], "reason": ""}
```

The block declares paths, not content: it is never a prompt to review what the program writes, and declaring a path grants nothing outside the current run. See [security.md](security.md) for the decision lifecycle.

### Authorization outcomes

When an `access` block requests writes, one decision is made before any JavaScript starts, and it is journaled as a bounded `run/access` event carrying the resolved paths, the reason, and the decision:

- `covered` — every requested target already matches an operator `--allow-write` scope; no prompt, JavaScript runs.
- `allow` — the user approved the set as one decision; JavaScript runs.
- `deny` / `cancel` — the user denied or cancelled the set; no JavaScript runs.
- `unavailable` — no interactive authorizer exists (pipe, CI, background run); no JavaScript runs.
- `invalid` — the request violated the path or bounds rules above; no JavaScript runs.
- `declared` — under `full-access` the declaration is accepted, journaled, and ignored; JavaScript runs.

In `read-only` mode a non-empty request is denied outright. Blocking decisions (`deny`, `cancel`, `unavailable`, `invalid`, and read-only denial) produce one bounded model observation stating the resolved set, the decision, and the next move: do not re-request the same set within this turn; continue read-only or hand off to the user. The journal is an audit record; authority is never restored from it.

In agent mode, a successful program must return exactly one tagged disposition:

- `{to: "model", facts: {...}}` ends the current JavaScript run and continues the same user turn. `facts` is a bounded object that serializes to at most 16384 bytes for the next model step.
- `{to: "user", message: "..."}` ends the current turn and prints the message to the user. Use this only when the result is established or a specific user action, missing input, authorization, or decision is required.

A normal top-level `return` releases the run's local JavaScript state; it does not by itself finish a turn. A format, parse, traversal, validation, timeout, or other recoverable operation error is model feedback, not an automatic user handoff. The next step should correct the operation, narrow the scope, or gather the missing evidence. A `catch` block that merely reports an error must return short facts to `to: "model"`, not hand control to the user. Only a real need for user input, authorization, or a decision belongs in `to: "user"`.

## Context budget

A run has two data channels. Program-provided data enters the next model context only through `to: "model"` `facts`; local variables, `print` output, and a `to: "user"` message do not become next-step model facts. The host may add bounded status, errors, and write receipts as trusted evidence. Keep `facts` to decision-relevant paths, counts, statuses, and bounded samples. Do not return complete scan results, whole file contents, or large arrays. If a large result must survive the run, write it to an authorized file and return only its path, count, and short summary. The 24 KiB result limit and 16 KiB facts limit are hard boundaries, not targets.

## Run semantics

Every program is wrapped and evaluated as one async function body. Top-level `return` and `await` are therefore legal in every run. The kernel does not infer execution mode from source shape and does not use an implicit last-expression result.

Each run uses a fresh QuickJS runtime with a 64 MiB heap, 1 MiB stack, bounded stdout, bounded host reads, and a validated deadline. There is no virtual path namespace: every path in a program is one absolute path in the operating-system user's filesystem view. Reads see exactly what the current OS user can read. Writes are checked against the invocation's frozen `RunFilesystemAuthority` — `read-only` denies every write, `planned-write` requires the resolved target identity to match an approved exact file or an operator-declared prefix, `full-access` keeps only path validation and the OS user's own permissions. That authority is fixed before QuickJS starts and cannot widen during the run.

## Runtime state

The system prompt begins with a byte-stable prefix: the same role text and host contract for every invocation, with no interpolated mode, path, timestamp, or model name. Per-invocation facts travel instead in a runtime-state block prepended to each new user message:

```text
<terrarium-runtime-state>
### Current runtime
- Working root: `/home/me/proj`
- Filesystem mode: `planned-write`
- Default run timeout: 10000 ms (hard cap 300000 ms)
- Installed host capabilities: `host.fs`
</terrarium-runtime-state>
```

Field names, order, and formatting are fixed. Values that could close the wrapper are escaped by the host. The block is historical text in the journal, never authority: a resumed session gets a fresh block for the current invocation's mode.

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

Host calls reject with a useful error instead of silently producing an empty result. In particular, scan traversal, file opening, and UTF-8 decoding failures carry the requested path. Scan option fields reject when their types are wrong. Hidden entries, symlinks, binary files, and `.gitignore` matches are intentional scan exclusions, not errors. `walk` shares scan's traversal engine and option set; it yields `{file, size}` entries and reports traversal failures only, since it never opens files.

A failed JavaScript run produces a compact model observation containing turn and step coordinates, status, termination, timeout, elapsed time, a bounded error classification, and any write receipts committed before the failure. It does not automatically copy the returned value or stdout into model context. A successful `to: "model"` disposition produces an observation containing the same coordinates, its bounded facts, and any write receipts. A successful `to: "user"` disposition produces no model observation; the outer loop appends `turn/end` with `reason: "handed_off"` and prints the message.

## LLM payload scope

The main model request is owned by the trusted outer agent loop. Each request uses the current turn's frozen resolved profile and is persisted as `model/request` before one network dispatch; a retryable attempt-1 failure may create exactly one attempt 2. Failed model results are not projected into later model-visible history.

Requests stream over server-sent events under three wire protocols — `openai-chat-completions`, `openai-responses`, and `anthropic-messages` — with a per-attempt total timeout and an inter-chunk idle timeout. Assistant reasoning (DeepSeek-style `reasoning_content`, Responses encrypted reasoning items, or Anthropic signed thinking blocks) is journaled with each successful `model/result` and replayed on every later request in the shape its protocol requires; foreign payloads are skipped when a session resumes under a different protocol. Per-request token usage (net input, output, cache read, cache write, reasoning) is journaled alongside it and reported as a context-budget line against the profile's declared `context_window`.

JavaScript has no `host.llm.call` or other in-program model-call primitive. The host surface is filesystem capabilities plus the tagged return protocol for continuing or handing off a turn. The configured model ID is sent unchanged to the provider endpoint. Requests are text-only; image file reading, encoding, and multimodal content parts are not implemented.

TOML provider/profile configuration and durable JSONL sessions are implemented. The session stores no credential value and no authority: the filesystem mode and write scopes are selected by the current invocation and are never restored from the journal.
