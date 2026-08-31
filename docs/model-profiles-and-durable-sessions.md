# Model Profiles and Durable Sessions

Status: implemented version 1 behavior. This document defines the current product and runtime semantics.

## 1. Why this exists

Terrarium needs two capabilities before it can be useful with real models:

1. named model profiles, so a user chooses one complete model setup instead of repeating endpoint details at every invocation;
2. durable sessions, so a process restart does not erase the conversation or repeat a JavaScript program whose effects are unknown.

The design follows the actual shape of Terrarium work. A session is usually one turn and rarely more than a dozen turns. Most activity happens inside a turn: the main model takes several steps and Terrarium executes several programs.

The persistent format should optimize for that shape, not for hypothetical sessions with millions of rows. Repeating one small resolved profile in each turn is acceptable when it removes registries, hashes, migrations, and shared-object rules.

The session file has one purpose: resume the session faithfully. It is not a database, search index, authorization token, tamper-evident audit log, telemetry format, or secret store. It records the working root as session context, but never stores an access mode or permission grant. On every invocation, the trusted host selects the current access policy independently of the journal.

## 2. Design philosophy

### 2.1 Minimize the whole system

A component is not simple merely because it hides complexity behind a dependency. Terrarium counts binary size, dependencies, data formats, failure modes, migration rules, and operational files as part of the system.

One append-only JSONL file uses serialization Terrarium already has and can be inspected with ordinary text tools. Version 1 therefore adds no database, schema migration framework, index, or auxiliary storage service.

### 2.2 Keep the user vocabulary small

The normal command starts the model-driven agent in the current directory. Users may choose a profile name without selecting a separate execution mode:

```sh
terrarium --profile main "review this project"
```

Providers and protocols remain host configuration details. There is no hidden task classifier, automatic model router, fallback chain, or provider-specific option bag at the call site.

Direct JavaScript execution is a distinct development and integration entry point:

```sh
terrarium run -e 'return await host.fs.text("/work/project/Cargo.toml")'
```

The main command is not overloaded by guessing whether its argument is a task, a task file, or JavaScript source.

### 2.3 One turn, one model binding

A turn resolves exactly one profile. That resolved profile remains fixed for every main-model step and retry in the turn.

Terrarium does not expose `host.llm.call` or another in-program model-call primitive in this version. An independent delegated model should eventually be an agent with its own lifecycle, conversation, budget, cancellation, permissions, and result contract. A stateless helper call is not a partial substitute for that design.

Removing this primitive has direct consequences:

- a turn stores one resolved profile, not a catalog;
- the main model sees no profile directory;
- JavaScript has no model-selection API;
- session recovery only tracks the main model and JavaScript runs.

### 2.4 Store facts, not framework objects

The journal stores:

- each user turn;
- the exact system prompt and resolved profile used by that turn;
- main-model request attempts and results;
- JavaScript start and result boundaries;
- the exact observations shown to the model;
- the terminal state of each turn.

It does not introduce durable binding objects, binding hashes, catalogs, separate IDs for every entity, branches, projections, or migrations without a concrete consumer.

Event sequence numbers are already unique inside one journal. They are used for internal references.

### 2.5 Prefer local duplication over global indirection

Each turn stores its complete resolved profile and exact system prompt. Sessions are short, so this is a small amount of repeated JSON. In return:

- every turn is self-contained;
- an open turn does not need the config file;
- config changes cannot silently alter a turn;
- no binding registry, canonical hash, or profile deduplication is needed.

### 2.6 Persist only boundaries needed for recovery

At the session level, only two operations require write-ahead records:

1. a main-model request attempt;
2. a JavaScript run.

A request attempt must be recorded before it may contact the provider. A run must be recorded before JavaScript may execute.

Pure transformations are stored with the result that produced them. Parsing an assistant response does not get a separate event. Formatting a run observation does not get a separate event.

### 2.7 Retry the step, not the history

A step is one logical main-agent decision. It may make attempt 1 and, after a retryable failure, attempt 2. Both attempts use the same frozen conversation and resolved profile.

The failed attempt remains in the journal but never enters model-visible conversation history. Attempt 2 does not consume another step. There is no third attempt in version 1.

The HTTP transport performs no hidden retry. One journaled attempt authorizes at most one network dispatch.

### 2.8 Never replay uncertain local effects

A main-model request can be attempted again because it does not repeat local filesystem effects, although it may duplicate provider work or cost.

A JavaScript program is different. If `run/start` is durable but `run/result` is missing, the program may already have changed files. Terrarium records an unknown outcome and tells the model to inspect current state. It never executes that source again.

### 2.9 Bind a session to its working root

A new local session binds its working root to the process working directory from which it is created. The binding contains:

- a user-facing absolute path used in prompts, observations, and answers;
- a canonical host path used only for containment and authority checks.

There is no virtual path namespace: programs use the operating-system user's absolute paths directly, and the runtime-state block names the session working root so the model and user speak about the same paths. The host canonicalizes symlinks and platform aliases internally for identity checks. Resuming from another shell directory never changes the stored working root.

The session working root is one stable resource identity for the life of the session. Version 1 does not persist attachment registries, write scopes, virtual path aliases, or per-directory ACLs. A task that fundamentally belongs to another root starts another session.

Each invocation selects one filesystem mode — `read-only`, `planned-write` (the agent default), or `full-access` — plus optional operator `--allow-write DIR|FILE` scopes in `planned-write`. These are selected by the trusted host and remain fixed for that invocation, but are not stored in the journal. In `planned-write`, a run's writes are preauthorized through the `access` block; see `filesystem-authorization.md`. `--full-access` allows real absolute paths visible to the current operating-system user, including paths outside the session working root. JavaScript does not expand `~`; the runtime-state block names the working root. A denied path is an authorization result, not a cue to guess alternate paths or invent scopes.

Direct `terrarium run` execution creates no session. Its transient working root is the process working directory for that invocation and is never persisted.

### 2.10 Keep invocation access coarse and honest

Each process invocation selects one filesystem mode (the normative contract is `filesystem-authorization.md`):

| Mode | Reads | Writes |
|---|---|---|
| `read-only` | OS-readable absolute paths | every write denied |
| `planned-write` (agent default) | OS-readable absolute paths | per-run preauthorized exact files plus operator `--allow-write` scopes |
| `full-access` | OS-readable absolute paths | any valid path, subject to OS permissions |

The mode belongs to the trusted host invocation, not to the session, turn, journal, prompt, or model. Resuming a session does not restore or imply a previous invocation's authority.

The name `full-access` is intentionally broader than "scoped files." A future unrestricted shell or external-process capability can escape any scope through subprocesses, hooks, build scripts, environment access, or networking. Such an operation may succeed only in `full-access` unless Terrarium can enforce the narrower boundary with a real operating-system or container sandbox. A write-scope check alone is not a security boundary for hostile execution.

Full access remains bounded by the operating-system user, run deadlines, resource budgets, and the capabilities installed by the host. It is not root access and does not remove execution limits. Version 1 does not implement shell or external-process execution; this rule fixes the permission boundary before such a capability exists.

The generated host contract exposes `host.fs.list(dir)` as sorted objects with `name`, `type`, and `size` fields. `type` is `file`, `directory`, `symlink`, or `other`; `size` is the byte count for regular files and `null` otherwise. Programs inspect these fields directly and do not parse display strings.

## 3. Terms

**Config** is one loaded TOML document containing providers, profiles, and a default profile.

**Provider** supplies a network base URL and an optional environment-variable name from which a credential is read.

**Protocol** is a built-in request/response codec. It owns endpoint path construction, authentication shape, request encoding, response decoding, and reasoning-effort mapping.

**Profile** is a named model-calling preset. It combines a provider, protocol, upstream model ID, optional output-token limit, and optional reasoning effort.

**Resolved profile** is the non-secret call specification obtained by resolving one profile against one config. A turn stores this value directly.

**Session** is one durable conversation stored in one JSONL file.

**Turn** begins with one user message and ends with an explicit user handoff, cancellation, step-limit exhaustion, or terminal failure. A successful `{to: "model", facts: {...}}` disposition keeps the turn open. A completed turn does not complete the session.

**Step** is one logical main-agent decision inside a turn. A step ends after its model response and either a JavaScript run result, a protocol observation, or a recoverable error observation. A `{to: "model", facts: {...}}` run result starts the next step in the same turn. A `{to: "user", message: "..."}` run result ends the turn by handing control back to the user. A terminal model failure may end the turn before the step succeeds.

**Attempt** is one durable try at a step. It authorizes at most one network dispatch. If the process stops after recording an attempt, the journal cannot know whether dispatch occurred, so that attempt remains consumed and its provider outcome is unknown.

**Run** is one fenced Terrarium JavaScript program selected by a successful model response.

**Working root** is the directory the session is anchored to. An agent session binds it when the session is created: its user-facing absolute path is named in the runtime-state block shared by the model and user, while its canonical host path is used for identity checks. A direct `terrarium run` invocation uses its current process directory transiently. Programs always use absolute paths; the working root is context, not a containment boundary for reads.

**Filesystem mode** is the invocation-wide `read-only`, `planned-write`, or `full-access` host policy, plus operator write scopes in `planned-write`. It governs writes and is never restored from session data.

**State reconstruction** means reducing stored events into session state and model-visible conversation. It never means reissuing a completed request or re-executing historical JavaScript.

## 4. Scope

Version 1 includes:

- one versioned TOML configuration file;
- named providers and profiles;
- three built-in wire protocols: OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages;
- text input and text output only;
- optional `low`, `medium`, or `high` reasoning effort;
- deterministic profile resolution when a turn begins;
- one fixed resolved profile throughout a turn;
- one append-only JSONL file per session;
- multiple user turns in one session;
- at most one retry for each main-agent step;
- continuation of interrupted open turns;
- conservative recovery that never repeats an uncertain run;
- one working root bound to the session at creation;
- one invocation-wide `read-only`, `planned-write`, or `full-access` filesystem mode (plus operator write scopes) that is never loaded from the journal;
- validation of the working root through the existing identity-resolution boundary for the session's display context;
- the model-driven agent as the default CLI entry point and `terrarium run` as the direct JavaScript entry point.

Version 1 excludes unused capability metadata. There is no context-window, image-modality, or video-modality field until Terrarium has context management or non-text payloads that consume those values.

## 5. Configuration

### 5.1 Discovery

The normal file is:

```text
$XDG_CONFIG_HOME/terrarium/config.toml
```

When `XDG_CONFIG_HOME` is unset on Unix, the fallback is `~/.config/terrarium/config.toml`. Other platforms use their normal per-user config directory with the `terrarium/config.toml` suffix.

Configuration selection precedence is:

1. explicit `--config PATH`;
2. the default per-user file, if it exists;
3. legacy `TERRARIUM_LLM_*` environment variables, if no TOML file was selected.

These sources are alternatives, not merge layers. An explicit or discovered TOML file that is invalid produces an error. Terrarium does not fall back to another source.

Includes, inheritance, overlays, directory scanning, project-local discovery, and `.env` loading are outside this specification.

### 5.2 TOML schema

```toml
version = 1
default_profile = "main"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"

[providers.local]
base_url = "http://127.0.0.1:11434/v1"

[profiles.main]
provider = "openrouter"
protocol = "openai-chat-completions"
model = "anthropic/claude-sonnet-4"
max_output_tokens = 32000
reasoning_effort = "high"

[profiles.fast]
provider = "openrouter"
protocol = "openai-chat-completions"
model = "google/gemini-flash"
max_output_tokens = 16000
reasoning_effort = "low"

[profiles.local]
provider = "local"
protocol = "openai-chat-completions"
model = "qwen3-coder"
```

`version` is required and must equal `1`.

`default_profile` is required and must name a profile.

Provider fields are:

| Field | Required | Meaning |
|---|---:|---|
| `base_url` | yes | HTTP or HTTPS service root to which the protocol appends its path |
| `api_key_env` | no | Environment-variable name containing the credential |

Profile fields are:

| Field | Required | Meaning |
|---|---:|---|
| `provider` | yes | Provider name |
| `protocol` | yes | Built-in protocol identifier |
| `model` | yes | Exact upstream model ID |
| `max_output_tokens` | no | Positive requested output-token limit |
| `reasoning_effort` | no | `low`, `medium`, or `high` |
| `request_timeout_ms` | no | Positive whole-attempt budget for one model call (default 300000) |
| `idle_timeout_ms` | no | Positive gap allowed between stream chunks before the attempt fails (default 120000) |
| `context_window` | no | Positive declared context window in tokens, used for budget reporting only |

Provider and profile names must match:

```text
[A-Za-z0-9][A-Za-z0-9._-]*
```

`base_url` must use HTTP or HTTPS, contain no credentials, query, or fragment, and be normalized without a trailing slash.

`model` is sent upstream unchanged. When present, `max_output_tokens` must be positive.

Unknown fields are errors. Strict parsing keeps misspelled options observable.

A missing credential does not prevent config loading when no selected turn uses that provider. It becomes a configuration error when a request needs it. Omitting `api_key_env` means the endpoint is intentionally unauthenticated.

The legacy environment loader creates one profile named `default`, uses `openai-chat-completions`, and leaves the optional limit and reasoning effort absent. It may remove one final `/chat/completions` segment from the legacy endpoint to produce a provider base URL. Other endpoint shapes are not guessed.

## 6. Protocol boundary

The core uses provider-neutral values equivalent to:

```text
ModelRequest
|-- messages: ordered text role/content messages, each optionally carrying reasoning
|-- model
|-- max_output_tokens | absent
`-- reasoning_effort | absent

ModelResponse
|-- content: text
|-- reasoning | absent  (text plus a protocol-tagged opaque replay payload)
`-- usage: input, output, cache-read, cache-write, and reasoning token counts
```

A protocol owns:

- the path appended to the provider base URL;
- authentication header shape;
- request JSON encoding;
- reasoning-effort encoding;
- success response validation and decoding.

The shared transport owns:

- the HTTP client;
- the per-attempt total timeout and the inter-chunk idle timeout;
- SSE byte decoding;
- response body limits;
- credential lookup;
- bounded error presentation.

One transport call attempts at most one network dispatch and never retries. A failure before dispatch and a failure after dispatch both belong to the same durable attempt. After a crash, Terrarium does not claim it can distinguish them.

All three protocols stream over server-sent events. Each request is encoded from the neutral projection, and each stream is decoded into text deltas, reasoning deltas, and a final usage snapshot:

| Protocol | Path | Auth | Reasoning replay | Reasoning enablement |
|---|---|---|---|---|
| `openai-chat-completions` | `{base}/chat/completions` | bearer | assistant `reasoning_content` field (DeepSeek style; the emitting field name is remembered) | `reasoning_effort` when configured |
| `openai-responses` | `{base}/responses` | bearer | stored reasoning items replayed verbatim into `input`, with `store: false` and `include: ["reasoning.encrypted_content"]` | `reasoning: {effort, summary}` when configured |
| `anthropic-messages` | `{base}/v1/messages` | `x-api-key` + `anthropic-version` | `{type: "thinking", thinking, signature}` blocks; `redacted_thinking` replays its opaque data; unsigned thinking is dropped rather than leaked as text | `thinking: {type: "enabled", budget_tokens}` when configured (low/medium/high map to 2048/8192/16384) |

Chat Completions maps `max_output_tokens` to `max_tokens`, Responses to `max_output_tokens`, and Anthropic requires `max_tokens` (defaulting to 8192 when unset). Chat Completions requests `stream_options: {include_usage: true}`.

Reasoning replay payloads are tagged with the protocol that produced them. A session resumed under a different protocol or model skips foreign payloads instead of corrupting the request. Provider usage objects are normalized to net input tokens (cache shares subtracted), cache-read and cache-write tokens, output tokens, and reasoning tokens.

Tool calls, provider-managed conversation state, and multimodal content are unsupported. An explicit reasoning effort must either be encoded by the selected protocol or make the profile invalid. It is never silently dropped.

## 7. Profile selection and turn snapshots

### 7.1 New session

A new local session records the current working directory as its working root. A normal new session therefore needs no path argument.

The invocation selects its filesystem mode independently:

1. `--read-only` -> `read-only`;
2. `--full-access` -> `full-access`;
3. `--allow-write DIR|FILE` (repeatable) -> `planned-write` with those operator scopes;
4. otherwise `planned-write` with no operator scopes (agent) or `read-only` (direct run).

`--read-only`, `--full-access`, and `--allow-write` cannot be combined. The selected mode and scopes are not written to the session.

The new session selects its first profile from:

1. explicit `--profile NAME`;
2. otherwise `default_profile`.

Only the selected profile is resolved. A missing credential value does not block resolution because the credential is read only when a request is made.

### 7.2 What a turn freezes

Each `turn/start` stores:

- the user message;
- the exact final system prompt sent to the model, including the stable working-root contract;
- the selected profile name;
- the complete resolved profile;
- the step and run-timeout limits used by the turn.

The resolved profile contains only values needed for future requests:

```text
name
protocol
base_url
api_key_env | absent
model
max_output_tokens | absent
reasoning_effort | absent
request_timeout_ms | absent
idle_timeout_ms | absent
context_window | absent
```

The provider name has already served its configuration purpose and is not repeated. The API key value is never stored.

The resolved profile is intentionally repeated in each turn. Sessions have few turns, and a little repeated JSON is simpler than binding registration, content hashes, shared objects, or migration rules.

### 7.3 Later turns

When a completed session receives another user message:

- with `--profile NAME`, Terrarium loads current config, resolves the named profile, renders the system prompt from current prompt assets, and stores the new snapshot;
- without `--profile`, Terrarium copies the previous turn's selected and resolved profile and turn limits; the system prompt stays byte-stable and the current invocation's runtime state rides on user messages; it does not read config;

The system prompt is byte-stable: the same role text and host contract for every invocation, with no interpolated mode, path, or model value. Per-invocation facts — working root, filesystem mode, default run timeout, installed capabilities — travel in the `<terrarium-runtime-state>` block at the head of each user message and never grant authority: access checks remain host policy. Access denials are observable host results, not prompt policy. This keeps adoption of current model configuration and prompt assets explicit while allowing every invocation to enforce its own filesystem mode.

On resume, `--config` is valid only together with `--profile` while starting a new turn. `--read-only`, `--full-access`, and `--allow-write` select the current invocation's filesystem mode whether Terrarium continues an open turn or starts a new one; they never mutate journal state. Other invalid combinations are usage errors.

The journal version covers the event schema, conversation projection, protocol codec semantics, run-fence semantics, and host contract required to continue a session. A binary that cannot honor that version may inspect the file but must refuse execution.

## 8. Session JSONL

### 8.1 Purpose and location

A session is one file:

```text
$XDG_STATE_HOME/terrarium/sessions/<session-id>.jsonl
```

When `XDG_STATE_HOME` is unset on Unix, the fallback is `~/.local/state/terrarium/sessions`. Other platforms use their normal per-user state directory.

The file contains the conversation and execution state needed to resume. It is ordinary application state. Version 1 adds no database, index, encryption, redaction, journal-specific permission policy, or auxiliary metadata files.

API key values are never written.

### 8.2 Header

The first physical line is the session header:

```json
{"type":"session","version":1,"id":"ses_01...","workingRoot":{"displayPath":"/work/project","canonicalPath":"/work/project"}}
```

The header is not an event and has no `seq`. `displayPath` is an absolute path in the local user's naming context and must be representable as a JSON string. `canonicalPath` is the path produced by host canonicalization when the session is created.

Resume validates the stored directory by re-resolving its identity. It fails if the root no longer exists, resolves to a different canonical location, is not a directory, or cannot be represented on the current host. Terrarium does not silently adopt the current shell directory, retarget the root, or rewrite stored paths.

The stored root identifies the session's working context; it grants no access by itself. The current invocation decides whether that root is read-only or writable and whether full host capabilities are authorized. A future Web or multi-user service maps the session to a server-authorized project or isolated worker and must not activate an uploaded host path merely because a journal contains it.

### 8.3 Event envelope

Every later physical line is one event:

```json
{"type":"turn/start","seq":1,"data":{}}
```

Envelope fields are:

| Field | Meaning |
|---|---|
| `type` | Event name |
| `seq` | Contiguous session-local sequence beginning at 1 |
| `ts` | Optional wall-clock append time, epoch milliseconds; absent in journals written before the field existed |
| `data` | Event-specific object |

Sequence defines order. JSON fields use `camelCase`. `ts` is operator-facing forensics — per-step model latency and turn duration are readable directly from the journal — and is never projected into model-visible context.

Unknown event types, duplicate sequences, sequence gaps, malformed complete lines, and invalid event shapes are errors. Existing complete events are never rewritten. Only an incomplete final physical line may be removed during recovery.

### 8.4 Writing

Creating a session uses exclusive file creation, writes the complete header, and synchronizes the file before appending the first `turn/start`. The session ID is published only after that event is durable. A crash before then may leave an unreferenced header-only file; it contains no user turn and is not resumable. Version 1 may report it as incomplete but does not add a publication event, cleanup registry, or repair path.

Appending an event means:

1. serialize one compact JSON object;
2. write it as one complete line with a trailing newline;
3. flush and synchronize the file before acting on the event.

The same rule applies to every event. Model and program latency dominate the synchronization cost, and one rule is simpler than durability tiers.

One process owns an exclusive lock on the journal file while writing. A second writer fails instead of interleaving events. No sidecar lock file is required. Within the process, one journal writer assigns sequence numbers serially.

## 9. Event model

Version 1 has six event types:

```text
turn/start
model/request
model/result
run/start
run/result
turn/end
```

The `seq` of `model/request` identifies that request. The `seq` of `run/start` identifies that run. No separate request, run, or binding IDs are generated.

The object shapes below are normative. Unknown fields are errors in journal version 1. Fields described as absent are omitted rather than encoded as `null`, except where `Kernel::Outcome` itself uses `null` as part of its existing shape. Agent `run/result` events store a normalized tagged `disposition`; legacy `answer` fields may be read for compatibility but are never written by the current agent.

### 9.1 `turn/start`

```json
{
  "type": "turn/start",
  "seq": 1,
  "data": {
    "message": "Review this project",
    "systemPrompt": "...",
    "profile": {
      "name": "main",
      "protocol": "openai-chat-completions",
      "baseUrl": "https://openrouter.ai/api/v1",
      "apiKeyEnv": "OPENROUTER_API_KEY",
      "model": "anthropic/claude-sonnet-4",
      "maxOutputTokens": 32000,
      "reasoningEffort": "high"
    },
    "limits": {
      "maxSteps": 256,
      "defaultRunTimeoutMs": 10000,
      "maxRunTimeoutMs": 300000
    }
  }
}
```

Only one turn may be open. This event is durable before step 1 begins.

### 9.2 `model/request`

```json
{
  "type": "model/request",
  "seq": 2,
  "data": {
    "step": 1,
    "attempt": 1
  }
}
```

The request belongs to the current open turn and uses its stored profile.

The model input is the deterministic conversation projection of the journal prefix before this event. Request events and failed model results are excluded from that projection, so attempt 2 receives exactly the same input as attempt 1.

The event is durable before its attempt may perform at most one network dispatch. Recording it consumes the attempt even if the process stops before dispatch can be proven.

### 9.3 Successful `model/result`

A successful result contains the complete assistant text and the pure parsing result computed before the event is written.

For a valid run:

```json
{
  "type": "model/result",
  "seq": 3,
  "data": {
    "requestSeq": 2,
    "ok": true,
    "content": "```run\nreturn await host.fs.text('/work/project/Cargo.toml')\n```",
    "action": {
      "kind": "run",
      "source": "return await host.fs.text('/work/project/Cargo.toml')\n",
      "timeoutMs": 10000
    }
  }
}
```

A successful result also carries the transport's accounting: a `usage` object (`inputTokens`, `outputTokens`, `cacheReadTokens`, `cacheWriteTokens`, `reasoningTokens`, all net of cache double counting) and, when the provider emitted reasoning, a `reasoning` object with the thinking `text` and the protocol-tagged opaque `replay` payload. Both are optional so pre-existing journals remain valid.

For an invalid run fence, the action stores the exact observation that will be shown to the model:

```json
{
  "type": "model/result",
  "seq": 3,
  "data": {
    "requestSeq": 2,
    "ok": true,
    "content": "I could not inspect the project.",
    "action": {
      "kind": "observation",
      "message": "protocol error: no program was executed; send exactly one complete ```run program with no prose or other code block"
    }
  }
}
```

Parsing and observation formatting occur before writing the result because they have no external side effects. Storing their output removes the need for parser-version and feedback-formatter events.

### 9.4 Failed `model/result`

```json
{
  "type": "model/result",
  "seq": 3,
  "data": {
    "requestSeq": 2,
    "ok": false,
    "error": {
      "kind": "transport",
      "message": "request timed out",
      "retryable": true
    }
  }
}
```

Stable error kinds are `configuration`, `transport`, `http`, `protocol`, `timeout`, `cancelled`, and `interrupted`. `timeout` covers both the whole-attempt budget and the inter-chunk idle deadline; both are retryable on attempt 1.

A retryable failure on attempt 1 permits attempt 2 for the same step. Attempt 2 is final even when its error remains retryable.

### 9.5 `run/start`

```json
{
  "type": "run/start",
  "seq": 4,
  "data": {
    "modelResultSeq": 3
  }
}
```

The referenced model result must belong to the current turn and contain `action.kind == "run"`. That action already stores the exact source and timeout, so `run/start` does not duplicate them.

The event is durable before JavaScript begins executing. Its sequence identifies the run.

### 9.6 `run/result`

A normal result stores the full kernel outcome and the normalized agent disposition. A `to: "model"` disposition also stores the exact bounded observation added to the main conversation:

```json
{
  "type": "run/result",
  "seq": 5,
  "data": {
    "runSeq": 4,
    "status": "completed",
    "outcome": {
      "ok": true,
      "value": null,
      "stdout": "",
      "error": null,
      "termination": "returned",
      "timedOut": false,
      "elapsedMs": 20
    },
    "disposition": {
      "to": "model",
      "facts": {"matches": [{"file": "/work/project/src/llm.rs", "line": 12}]}
    },
    "observation": "{\"turn\":1,\"step\":1,\"to\":\"model\",\"facts\":{\"matches\":[{\"file\":\"/work/project/src/llm.rs\",\"line\":12}]}}"
  }
}
```

A `to: "user"` disposition stores the user-facing message and omits `observation`:

```json
{
  "type": "run/result",
  "seq": 8,
  "data": {
    "runSeq": 7,
    "status": "completed",
    "outcome": {
      "ok": true,
      "value": null,
      "stdout": "",
      "error": null,
      "termination": "returned",
      "timedOut": false,
      "elapsedMs": 8
    },
    "disposition": {
      "to": "user",
      "message": "The HTTP client is configured in src/llm.rs."
    }
  }
}
```

A failed run stores a compact model observation with turn and step coordinates, status, termination, timeout, elapsed time, and bounded error information. It does not automatically copy its return value or stdout into model context.

Recovery uses a distinct variant when `run/start` exists but no result does:

```json
{
  "type": "run/result",
  "seq": 5,
  "data": {
    "runSeq": 4,
    "status": "outcome_unknown",
    "observation": "the previous program may have changed state before the process stopped; it was not repeated, so inspect current state before proceeding"
  }
}
```

`outcome_unknown` is recovery state, not a fabricated kernel outcome.

### 9.7 `turn/end`

A user handoff appends `turn/end`:

```json
{
  "type": "turn/end",
  "seq": 9,
  "data": {
    "reason": "handed_off",
    "handoffRunSeq": 8
  }
}
```

Reasons are:

- `handed_off` for a completed `to: "user"` disposition;
- `step_limit`;
- `failed`;
- `cancelled`.

`handoffRunSeq` is required only for `handed_off` and must reference a completed run whose disposition targets `user`. The legacy `answered` reason and `answerRunSeq` field remain readable for older journals but are not written by the current agent.

## 10. Conversation projection

Only events before the request are considered. Projection starts with the current turn's exact stored `systemPrompt`, then traverses prior events in `seq` order and emits:

- each `turn/start.data.message` as a user message;
- each successful `model/result.data.content` as an assistant message, restoring its stored `reasoning` so the active protocol replays it in its own shape;
- each `model/result.data.action.message` as a protocol observation;
- each `run/result.data.observation` as a run or recovery observation.

Projection excludes:

- resolved profile connection details;
- request and retry metadata;
- failed model results;
- credentials, which are never stored.

Because a failed attempt contributes no conversation message and no other conversation event may occur between attempts, attempt 2 reconstructs the exact input used by attempt 1 without storing a second message copy or context identifier.

Version 1 does not truncate, summarize, or compact history. Each model call reports the projected context footprint (input plus cache plus output tokens against the profile's declared `context_window`) to the operator and journals the per-request usage, but takes no automatic action. If a provider rejects an oversized request, Terrarium reports the error. A future compaction feature must add explicit state rather than rewrite history.

## 11. Step execution and retry

For each new step:

1. reconstruct the model-visible conversation from the journal;
2. append `model/request` with `attempt: 1`;
3. attempt at most one network dispatch;
4. append `model/result`;
5. on a retryable failure, append attempt 2 for the same step;
6. on success, process the stored action;
7. end the step after its protocol observation, run result, recoverable error observation, or explicit user handoff;
8. increment the step only when another main-agent decision is needed.

Attempts for one step never overlap. Attempt 2 must follow a retryable failed result for attempt 1. There is no attempt 3.

A retry does not consume another step. If attempt 2 fails, the turn ends with `failed`. A non-retryable error also ends it with `failed`, except operator cancellation ends it with `cancelled`.

Retryable failures are:

- transport failures;
- HTTP 429;
- HTTP 5xx;
- interruption after a request event but before its result was durable.

Configuration errors, HTTP 4xx other than 429, and invalid provider responses are non-retryable.

A timeout can leave the provider outcome unknown. Attempt 2 may duplicate provider work or cost. Terrarium records the uncertainty and never makes a third attempt.

After a successful `to: "model"` step, or after a recoverable protocol or run error, Terrarium begins the next step unless doing so would exceed the turn's stored `maxSteps`. A successful `to: "user"` disposition ends the turn with `reason: "handed_off"`. At the step limit it appends `turn/end` with `reason: "step_limit"`.

## 12. Resume and recovery

Conceptual CLI forms are:

```sh
terrarium [--config PATH] [--profile NAME] [--read-only | --full-access | --allow-write DIR|FILE]... <message...>
terrarium --resume SESSION_ID [--read-only | --full-access | --allow-write DIR|FILE]...
terrarium --resume SESSION_ID [--config PATH] [--profile NAME] [--read-only | --full-access | --allow-write DIR|FILE]... <message...>
terrarium run [-e SOURCE | FILE] [--read-only | --full-access | --allow-write DIR|FILE]... [--timeout-ms N]
```

The main command is always the model-driven agent. Message arguments are joined as text; Terrarium does not reinterpret an existing path as a task file. With no message, non-terminal stdin may supply the message. The filesystem mode flags apply to every run in that invocation. In `--full-access`, writes need only valid paths and the current operating-system user's permissions; JavaScript does not expand `~`, so the model must use the actual absolute home path named in the runtime state. `terrarium run` is the only direct JavaScript entry point: it reads source from `-e`, one file, or non-terminal stdin, creates no durable session, defaults to read-only, and emits one structured outcome.

Rules are:

- without `--resume`, the agent creates a session rooted at the current working directory and starts its first turn;
- `--resume ID` without a message continues an open turn;
- resume without a message fails when no turn is open;
- resume with a message requires the previous turn to be closed and starts a new turn;
- `--profile` is valid only while starting a new turn;
- the stored working root, prompt, profile, and limits belong to the session or open turn; the access mode belongs only to the current invocation;
- when no mode flag is supplied, every new or resumed agent invocation uses `planned-write` with no operator scopes, regardless of modes used by earlier invocations;
- completed turns are never reopened;
- the session ID is printed to stderr when a session is created, leaving stdout as the answer channel.

Recovery removes at most one incomplete final physical line, validates all complete events, reduces the journal, and handles the final state as follows.

### 12.1 Turn started, no request

Begin step 1. For a later step, the preceding stored observation determines the next step number.

### 12.2 Request has no result

The provider outcome is unknown. Recovery appends a failed `model/result` with `kind: "interrupted"` and `retryable: true`.

If the request was attempt 1, recovery issues attempt 2 for the same step. If it was attempt 2, recovery ends the turn with `failed`.

### 12.3 Request has a failed result

A retryable failure from attempt 1 produces attempt 2. Any failure from attempt 2 ends the turn with `failed`. A non-retryable attempt-1 failure also ends the turn with `failed`. Cancellation ends it with `cancelled`.

### 12.4 Successful model result has a run action but no `run/start`

The program has not crossed its execution boundary. Recovery appends `run/start` and executes it for the first time under the current invocation's access policy.

The model request is not repeated. If the current policy denies an operation that a previous invocation would have allowed, the run records that ordinary observable failure; recovery never obtains authority from the journal.

### 12.5 `run/start` has no result

The program may have changed files. Recovery never executes it again.

It appends `run/result` with `status: "outcome_unknown"` and the exact recovery observation, then advances to the next step or closes at the step limit.

### 12.6 Completed run has no continuation

If its disposition targets `user`, append `turn/end` with `reason: "handed_off"` and the referenced `handoffRunSeq`. Print the disposition message once. A `to: "model"` disposition, a compact run-error observation, or a protocol observation continues to the next step, or closes at the step limit. For compatibility, an older completed run with `outcome.answer` but no disposition is closed with the legacy `answered` reason.

### 12.7 Protocol observation has no continuation

The observation is already stored in `model/result`. Begin the next step, or close at the step limit.

## 13. Journal invariants

A version 1 journal is valid only when:

- the header is first and appears once;
- an executable journal contains at least one `turn/start`; a header-only file is incomplete and not resumable;
- event sequences are contiguous and begin at 1;
- at most one turn is open;
- events for a later turn never appear before the previous turn ends;
- every event reference points to an earlier compatible event;
- each model request has at most one result;
- main steps start at 1 and increment by one;
- attempts are limited to 1 and 2;
- attempt 2 follows a retryable failure from attempt 1 in the same step;
- at most one attempt per step succeeds;
- a successful model result contains exactly one action;
- one run action has at most one `run/start`;
- one run has at most one result;
- a completed `to: "model"` run has a model observation whose facts serialize to at most 16384 bytes;
- a `handed_off` turn references a completed run with a user disposition;
- no event follows `turn/end` until the next `turn/start`.

Validation reports the offending sequence and invariant. Terrarium does not silently discard complete events to make an invalid journal usable.

## 14. Errors, credentials, and ordinary file state

Configuration errors include the full field path. Selection errors include the requested profile and valid names. Session errors include the session ID and offending event sequence when available.

Provider errors expose bounded classifications and status information without copying unbounded response bodies.

API key values exist only in host process memory and request headers. They never appear in TOML, turn snapshots, journal events, JavaScript globals, prompts, or normal errors. The environment-variable name is stored because it is part of the resolved profile.

The journal may contain user prompts, source code, paths, model responses, program output, and answers. Version 1 adds no journal-specific permission policy, encryption, redaction, signing, or tamper detection. Protecting ordinary application-state files is the operating environment's responsibility, not part of durable-session semantics.

For the local CLI, the stored working root is trusted same-user application context: editing it can make the next default invocation name another directory in its runtime state. It is still not an authority grant, and it is not portable authority. A future service must replace local host-path trust with a server-controlled session-to-project or session-to-worker binding.

## 15. Acceptance requirements

All acceptance tests use a local mock HTTP server. No real third-party model service is required.

### 15.1 Profiles

- One TOML file defines multiple providers and profiles.
- Default and explicit profile selection are deterministic.
- Provider location and protocol encoding remain separate.
- Unknown fields and invalid references fail before network activity.
- A missing credential for an unselected provider does not block the selected profile.
- Exact model IDs are sent unchanged.
- A turn stores one complete resolved profile and never reads mutable config afterward.
- A later turn without explicit profile selection copies the previous profile, prompt, and limits.
- Explicit profile selection adopts current config and prompt assets without rewriting history.

### 15.2 Working root, access, and CLI

- A new local session stores the absolute display path and canonical path of its creation directory as one working root.
- Resume from another process directory keeps the stored root and never silently retargets it.
- Model programs use the same absolute working-root paths shown to the user, and user-facing observations and answers do not introduce an unrelated virtual namespace.
- `planned-write` is the default on every agent invocation; direct run defaults to `read-only`.
- `--read-only`, `--full-access`, and `--allow-write` never combine and never appear in the session header, turn data, or events.
- Editing a journal cannot select a filesystem mode or write scope; the live host policy decides every operation.
- An unrestricted shell or external-process operation is denied outside `full-access` unless a real sandbox constrains it to the narrower mode.
- `terrarium <message...>` enters the model-driven agent and never guesses that a message naming an existing file is a task file.
- `terrarium run` executes JavaScript from exactly one of `-e`, a source file, or non-terminal stdin without creating a session; its transient working root is the invocation's current directory.

### 15.3 Steps and retries

- One main step can have attempt 1 and attempt 2 with exactly the same model input.
- A retryable first failure creates attempt 2 without consuming another step.
- A failed attempt is absent from model-visible conversation.
- At most one attempt in a step succeeds.
- There is no attempt 3, including after restart.
- One journaled attempt authorizes at most one network dispatch.
- The transport performs no hidden retry.

### 15.4 Journal and projection

- A session persists multiple turns in one JSONL file.
- Reconstruction produces the exact user, assistant, protocol-observation, and run-observation order.
- Each turn is self-contained through its stored prompt and resolved profile.
- No binding registry, profile catalog, or binding hash is required.
- API key values never appear in the journal.
- The session header stores one stable working root and no access mode or permission grant.
- The stored working root is validated by identity resolution, but the current invocation decides the effective filesystem mode and write scopes.
- `planned-write` is the agent default; the mode flags and `--allow-write` scopes never persist into the journal.
- A second writer cannot interleave events.

### 15.5 Recovery

- An incomplete final line can be removed without losing the preceding event.
- A header-only file is reported as incomplete and cannot be resumed; its session ID was not published.
- A malformed complete line is rejected.
- An unmatched attempt 1 is marked interrupted and advances to attempt 2 at the same step.
- An unmatched attempt 2 is marked interrupted and terminates the turn.
- A saved run action without `run/start` re-derives its `run/access` decision under the current invocation policy and executes at most once.
- Resuming the same open turn under any filesystem mode never changes journal state and never grants authority from stored data.
- A host denial during recovered execution is stored as an ordinary run result and the source is not retried automatically.
- A run with durable `run/start` and no result is never re-executed.
- A stored run outcome or protocol observation continues without regenerating its text.
- A stored `to: "user"` disposition closes the turn exactly once with `handed_off`.
- A legacy answered turn remains closed.

## 16. Explicit non-goals

Version 1 does not include:

- `host.llm.call` or another in-program model-call primitive;
- delegated agents or sub-agents;
- shell or external-process execution;
- attachment registries, dynamic working roots, virtual path aliases, per-directory grants, or general ACLs;
- persistent access modes or permission events in the session journal;
- SQLite or another database;
- session indexes, search, listing UI, or deletion UI;
- journal-specific file permissions, encryption, redaction, signing, or hash chains;
- durable binding registries, binding hashes, profile catalogs, or profile deduplication;
- separate UUIDs for turns, requests, runs, or bindings;
- hidden model routing or fallback profiles;
- provider/model catalogs compiled into Terrarium;
- profile inheritance, merging, includes, or overlays;
- arbitrary provider headers or raw vendor JSON;
- automatic context management or compaction;
- image, video, audio, or artifact transport;
- changing the profile inside an open turn;
- persisting, inferring, or restoring an access mode from session data;
- replaying JavaScript with an unknown outcome;
- rollback, history rewriting, or branching;
- multi-user service authorization.

A future agent-delegation design must stand on its own. It must define lifecycle, conversation ownership, budgets, cancellation, permissions, structured results, persistence, and recursion limits instead of reviving a stateless helper call by default.

## 17. Resulting boundary

For users:

```text
configure providers and profiles once
start the agent in the directory that defines the session's working root
use planned-write by default, or select read-only, full access, or operator write scopes for this invocation
resume by session ID without retargeting the working root or restoring old permissions
use terrarium run only for direct JavaScript execution and testing
```

For the model:

```text
use the current turn's model binding and the session working root
emit one program per step
receive observable denials from the live host policy
never select providers, profiles, permissions, protocols, or persistence behavior
```

For the runtime:

```text
Config -> one resolved turn profile
Host invocation -> read-only | planned-write | full-access (+ write scopes, never journaled)
Session header -> one stable working root
               -> step -> visible attempt 1 -> optional attempt 2 -> model result
                       -> optional run/access decision -> run under frozen authority -> run result
               -> append-only JSONL
```

This is the intended balance: bind one understandable working root to the session, keep authority in the trusted invocation, make the agent the normal command and direct JavaScript an explicit `run` command, duplicate one small immutable profile per turn, and persist only the boundaries required for conservative recovery.
