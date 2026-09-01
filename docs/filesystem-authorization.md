# Filesystem Access and Write Preauthorization

Status: implemented. This document is the normative contract for filesystem access and write preauthorization; `protocol.md` and `security.md` summarize it. It replaces the earlier mount-based authorization model.

## 1. Scope

Terrarium gives model-written JavaScript a small filesystem API. This proposal defines a simpler target contract:

- programs use the user's absolute paths directly;
- reads use the operating-system user's readable filesystem view;
- writes are denied, preauthorized as an exact per-run file set, covered by an operator-declared write scope, or allowed under an explicit full-access mode;
- any user decision happens before QuickJS starts;
- actual write calls are still checked by the trusted host;
- the model receives current host state through a small runtime-state block at the head of each user-role message;
- the system prompt starts with one byte-stable prefix in every mode.

This version has no process, shell, command, or independent network capability. It does not design placeholders for capabilities that are not installed.

## 2. First principles

The design follows these rules:

1. The model proposes source code and requested write targets. It does not grant itself authority.
2. The trusted host owns the invocation mode, resolved paths, approval result, and actual filesystem effects.
3. Approval before execution is simpler than suspending a live JavaScript runtime for user input.
4. Preauthorization removes runtime interaction, but it never replaces checking the actual host call.
5. A filesystem path should mean the same thing to the user, model, and host-facing API.
6. Runtime facts change more often than core instructions and must not mutate the stable prompt prefix.
7. Mechanisms that have no current consumer are excluded.

Consequently, this proposal has no virtual path namespace, mounts, per-directory policy store, persistent permission database, runtime authorization wait, model-selected mode, model-requestable write scopes, static JavaScript analysis, or generic capability-authorization framework. The only recursive write scopes are operator-declared launch arguments.

## 3. Paths and working root

Every `host.fs` path is an absolute user path. There is no `/workspace` alias and no `virtual=real` mapping. Relative paths and `~` expansion are rejected.

`working_root` is the stable directory associated with an agent session, or the process working directory for a direct run. It is useful context for the model, but it is not a namespace and does not itself grant or restrict access.

For example, when the working root is `/code/terrarium`, the model uses:

```js
const cargo = host.fs.text("/code/terrarium/Cargo.toml");
```

It never has to translate that path to a synthetic name.

Reads in all modes use the operating-system user's readable view. Read failures remain observable host errors. A program can place read data in model-visible facts, so selecting a model provider also selects the trust boundary for data the program reads. This proposal deliberately does not describe broad reads as data isolation.

## 4. Invocation modes

The trusted caller selects exactly one filesystem mode for an invocation:

| Mode | Reads | Writes | User interaction |
|---|---|---|---|
| `read-only` | OS-readable absolute paths | always denied | none |
| `planned-write` | OS-readable absolute paths | exact files approved for this run, plus operator-declared scopes | one decision before execution for requested targets not already covered |
| `full-access` | OS-readable absolute paths | any absolute path allowed by the OS user | none |

`planned-write` is the default for the model-driven agent. `read-only` is a hard denial, not a mode that can be elevated by the model. `full-access` is an explicit operator choice intended for trusted CLI and debugging workflows.

In `planned-write`, the operator may grant recursive write scopes at launch with repeatable `--allow-write DIR|FILE`. A `FILE` argument matches exactly that resolved file; a `DIR` argument matches every path under it. Each operand must already exist at launch — an operator granting a scope for a tree the run itself will create makes the directory first — and resolves to one canonical identity, so aliases cannot widen or narrow it. Granted scopes join each run's frozen write authority without prompting, and requested targets they cover never reach a user prompt. The model cannot request, broaden, or discover these scopes: an access block declares exact files only, and the runtime state does not list granted scopes — the model always declares its intended targets and the host subtracts coverage. Passing `--allow-write` together with `--read-only` or `--full-access` is a launch error.

`full-access` applies only to installed filesystem capabilities. In this version it does not imply process execution or independent network access because those capabilities do not exist.

The mode is invocation-local. A session journal or historical runtime-state message is not authority and never restores a previous mode.

## 5. Stable prompt and runtime state

### 5.1 Stable prefix

The system prompt begins with one byte-stable core prefix. Modes, paths, timestamps, request identifiers, model names, authorization results, and other runtime values must not be interpolated into that prefix.

The prefix explains:

- the host API and run protocol;
- all three filesystem modes;
- the meaning of the runtime-state block;
- that only the mode named by the current host state applies;
- that this version installs no process-execution and no independent-network capability;
- that prompts and model output do not enforce permissions.

Within one prompt protocol version, existing prefix bytes never change. A breaking instruction change creates a new version instead of silently rewriting the old prefix. Capability-specific material may be appended after the stable prefix in deterministic order without changing the prefix itself.

### 5.2 User-message rendering

Every newly emitted user-role message is one text string whose head is a host-generated runtime-state block, followed by the natural user message or host observation as Markdown:

```text
user.content = "<terrarium-runtime-state>...</terrarium-runtime-state>\n\n" +
               "the user's Markdown or a host observation"
```

The protocol-neutral message model keeps a single content string, and all three adapters keep their current text encoding. No multi-part content representation is required. The concatenation is deterministic, so a rendered message is byte-stable and a retry of the same model request reuses exactly the same bytes.

The stable prompt states that a message beginning with the host state block carries host metadata; a same-named tag elsewhere in user text is ordinary user text. This convention improves model understanding; it is not a security boundary.

### 5.3 Runtime-state format

The runtime state is XML-wrapped Markdown, not an XML object model:

```xml
<terrarium-runtime-state>
### Current runtime
- Working root: `/code/terrarium`
- Filesystem mode: `planned-write`
- Installed host capabilities: `host.fs`
</terrarium-runtime-state>
```

The host owns every value and escapes text that could close the wrapper. Fields use fixed names and a deterministic order. The state contains no timestamp, random identifier, elapsed counter, or other value that changes without affecting model behavior. Facts that never change during a binary's lifetime — such as the absence of process or network capability — are stated once in the stable prefix, not repeated per message.

A complete state snapshot is included in every newly emitted user-role message, including host observations between model steps. When state is unchanged, the rendered block is byte-identical. When state changes, the host emits a new snapshot and never rewrites historical messages. The latest snapshot is current; older snapshots are historical context only.

This preserves the request prefix used by the previous model call while making every new step self-describing.

## 6. Write access request

### 6.1 Response shape

The term `plan` is intentionally avoided because a model may also produce an implementation plan or reasoning plan. A write access request is a narrow preauthorization protocol.

Every model-selected run is one closed `run` block, optionally preceded by one closed `access` block; any other arrangement is a protocol error:

````text
```access
{"writes":["/code/terrarium/src/fs.rs","/code/terrarium/tests/library_api.rs"],"reason":"Update filesystem authorization and its regression tests"}
```
```run
const source = host.fs.text("/code/terrarium/src/fs.rs");
host.fs.write("/code/terrarium/src/fs.rs", update(source));
return {to: "model", facts: {updated: true}};
```
````

The prompt instructs the model to always emit the `access` block, even the empty form, so the response habit is one unconditional rule. The parser is deliberately more forgiving than the instruction: an absent `access` block is the empty request, not a protocol error, because omitting an empty declaration is semantically identical to making it. Text outside the blocks is display-only. A protocol error exists only for genuine ambiguity: no `run` block, an unclosed block, an `access` block after the `run` block, more than one `access` block, more than one `run` block, or invalid access JSON.

The `access` value is strict JSON with exactly two fields:

```text
writes: array of exact absolute file paths
reason: one short user-facing string
```

When no preauthorization is needed, the model emits the fixed empty form:

```json
{"writes":[],"reason":""}
```

The request is data for the host. It is not JavaScript, a tool call, a general task plan, or a permission grant.

### 6.2 Meaning by mode

The same response grammar is used in every mode:

| Current mode | `writes` value | Host behavior |
|---|---|---|
| `read-only` | empty; a non-empty request is corrective feedback and JavaScript does not start | run under a hard write denial |
| `planned-write` | exact files the program may write, or empty for an inspection-only run | ask once before execution for requested targets not covered by an operator scope |
| `full-access` | any value; a non-empty list is journaled as declared intent and ignored for authority | run with full filesystem write authority and no prompt |

In `planned-write`, a non-empty `reason` is required when `writes` is non-empty. The reason is display text only. It never changes which paths are authorized.

In `full-access`, the declared set is still recorded with the run: comparing it later with committed write receipts is an audit signal for anomalous behavior, never an enforcement mechanism.

### 6.3 Path request rules

Each requested target must be:

- an absolute file path;
- free of `.` and `..` components and ambiguous separators;
- an exact path, not a directory, glob, prefix, or recursive scope;
- unique after host normalization.

A target may be an existing file or a new file. Approving a new-file target subsumes creating its missing parent directories as part of that one write; the approval display marks targets whose parents do not yet exist. Directory creation never extends authority to any other path: another file in a created directory still needs its own approval.

Existing symbolic-link targets are rejected for writes. Parent path aliases are resolved by the host before display and approval. Normalization is platform-aware: macOS `/tmp` aliasing and Windows drive-letter case and separator forms must collapse to one identity before uniqueness and membership are decided.

The host bounds the request to at most 32 paths, 4 KiB encoded, and a 200-character reason. An invalid or oversized request is a protocol error and JavaScript does not start.

## 7. Preauthorization lifecycle

For `planned-write`, the complete lifecycle is:

```text
model response
    -> parse an optional access block and exactly one run block
    -> validate and resolve every requested path
    -> subtract targets covered by operator-declared scopes
    -> present the remaining set and reason to the user
    -> user allows or denies that set as one decision
    -> freeze the run's write scopes: operator scopes plus approved exact paths
    -> start QuickJS
    -> check every actual write against those frozen scopes
```

If nothing remains after subtraction, no prompt occurs and the run starts directly.

The user decisions are only:

```text
allow
deny
```

Partial approval is not supported. Changing any requested path requires a new model response and a new decision. A denial ends the whole run, including targets an operator scope would have covered; the model may re-request only the covered subset in a new response, which will not prompt. Approval applies only to the associated run and is discarded when that run ends.

User interaction is an explicit adapter-owned interface, never kernel behavior:

```rust
trait Authorizer {
    fn decide(&self, request: &ResolvedAccessRequest) -> Decision;
}

enum Decision { Allow, Deny, Cancel, Unavailable }
```

The terminal adapter implements one decision prompt. An invocation with no interactive authorizer — a pipe, CI job, or background run — implements `unavailable`. The kernel receives only frozen authority and never renders a prompt.

If the request is denied, cancelled, or no authorizer is available, JavaScript does not start. The outer agent receives a bounded structured result — `authorization_denied`, `authorization_cancelled`, or `authorization_unavailable` — and the model observation states the resolved set that was not authorized, the decision, and what to do next: do not re-request the same set within this turn; continue read-only or hand off to the user. The unavailable variant states that no write can be authorized in this invocation.

Denial decisions are not cached in this version; the observation wording is the only loop guard. If durable journals later show a model repeatedly re-prompting an identical denied set, an in-turn suppression may be added inside the adapter without changing this interface.

There is no pending authorization inside QuickJS, no paused execution budget, no continuation to resume, and no runtime authorization timeout. The ordinary run timeout starts only after preauthorization succeeds and QuickJS starts.

## 8. Runtime enforcement

Preauthorization establishes a maximum write scope; it does not trust the program to obey its declaration.

Every `host.fs.write` and `host.fs.replace` call uses the path actually supplied by JavaScript:

```text
actual write call
    -> validate absolute path syntax
    -> resolve the existing parent and target state
    -> apply the invocation mode
    -> in planned-write, require the target to match a frozen scope:
       an approved exact path or an operator-declared prefix
    -> recheck symbolic-link and parent conditions
    -> perform the atomic write, creating approved missing parents
    -> record the ordinary write receipt
```

Mode enforcement is deterministic:

- `read-only` returns `write_denied` for every write;
- `planned-write` returns `write_not_authorized` when the actual target matches no frozen scope;
- `full-access` skips scope membership but keeps path validation and OS permission checks.

An undeclared path never opens a second prompt. JavaScript may catch the error, but alternate spelling, a symbolic link, or another host function cannot expand the frozen scopes.

Approval is path authorization, not content review. After a target is approved, the program may write any bounded text content to that exact target. A future diff preview would be a user-interface feature, not part of this permission model.

Multiple approved writes are not a transaction. Earlier committed writes remain if a later write fails. Host-derived write receipts continue to describe committed writes; they are not a rollback log.

Read operations remain synchronous host operations where they are synchronous today. Write operations do not need to become awaitable for authorization because user interaction is complete before QuickJS starts. Existing asynchronous scan and walk iteration remains unchanged.

The host continues to own regular-file checks, decoding rules, bounded reads, traversal behavior, atomic replacement, output bounds, cancellation, and receipts. This runtime does not claim to defeat a hostile concurrent process replacing filesystem objects between checks; that requires an operating-system isolation boundary outside this design.

## 9. Sessions, recovery, and authority

A runtime-state snapshot may appear in durable model-visible history so later requests can preserve the exact message prefix. It remains historical text, not authority. On resume, the trusted caller selects a fresh invocation mode and the next user-role message carries that current state.

Each access request and its decision are journaled as one bounded `run/access` event carrying the resolved paths, the reason, and the decision — including declarations accepted and ignored under `full-access`. The journal is an audit record, never authority.

A write access request and its approval are run-local. The first version does not persist reusable grants, pending approvals, an ACL database, or a permission registry. A run that had started but has no durable result remains `outcome_unknown` and is never replayed automatically.

These invariants hold:

- no JavaScript starts before a required access request is approved;
- no effect occurs at a path outside the mode's actual authority;
- denial or cancellation before execution has no program filesystem effect;
- an approved program is executed at most once by recovery logic;
- historical prompt text never restores authority.

## 10. Target architecture

The minimal trusted types are conceptually:

```rust
enum FilesystemMode {
    ReadOnly,
    PlannedWrite,
    FullAccess,
}

enum WriteScope {
    Exact(ResolvedPath),  // user-approved for one run
    Prefix(ResolvedPath), // operator-declared at launch
}

enum RunFilesystemAuthority {
    ReadOnly,
    Scoped(Vec<WriteScope>),
    FullAccess,
}
```

The outer agent adapter parses the model response and obtains any required user decision through an `Authorizer`. The reusable kernel receives only source code plus host-derived `RunFilesystemAuthority`; it does not parse model prose or implement a user interface.

The implementation direction is:

1. Render the runtime state as a deterministic host-prepended text block inside the existing single-string message content; adapters keep their current text encoding.
2. Split the prompt into an immutable core prefix and host-rendered runtime state.
3. Extend the run-block parser with an optional access block: absent means the empty request, at most one of each block, surrounding prose stays display-only.
4. Replace public `Mount` input with an invocation filesystem mode plus operator `--allow-write` scopes and, per run, an exact approved path set.
5. Replace mount resolution with direct absolute-path validation and platform-aware normalization.
6. Remove `--mount`; keep explicit `--read-only` and `--full-access`; add repeatable `--allow-write DIR|FILE`, valid only in `planned-write`; keep `planned-write` as the model-driven default.
7. Perform planned-write approval in the outer adapter through the `Authorizer` interface before calling the kernel, and journal each `run/access` decision.
8. Share one scope-membership check across `write` and `replace` and retain write receipts.
9. Update prompts, registry text, session projection, CLI output, tests, and normative documentation in the same contract version.

For direct `terrarium run`, the minimal useful choices are read-only by default and explicit `--full-access` for trusted debugging; `--allow-write` scopes are equally available for operator-authored programs. It has no model access block to preauthorize paths.

## 11. Explicit non-goals and future extension

This version does not implement or specify process execution, shell commands, child processes, independent network requests, persistent grants, per-directory policy, model-requestable wildcard or prefix write scopes, partial approval, content approval, transactions, rollback, or denial caching.

If a future version installs process or network capabilities, it may reuse the outer sequence of request, preauthorization, and deterministic host checking. It must add capability-specific runtime-state fields, request records, and matchers. File paths, process spawns, and network requests are not interchangeable authorization objects.

A future process request would need to match at least the resolved executable, complete argv, cwd, and environment policy. A future network request would need to match at least scheme, host, port, method, path, redirect policy, and outbound data policy. Neither should inherit authority merely because filesystem mode is `full-access`.

Such additions must preserve the existing stable prompt prefix or introduce a separately versioned successor. They should not add speculative fields to the current access block before the capabilities exist.
