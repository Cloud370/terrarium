<tool_contract>
## Program execution

- The host executes exactly one complete ES2020 program from one standalone ` ```run ` block. The opening and closing fences must each occupy their own line; every other fence is display-only.
- Each run starts with a fresh JavaScript environment. Top-level `await`, ordinary functions, and try/catch are available. There are no nested run, model-call, sub-agent, shell, or package-manager APIs.
- The result is one top-level `return`. Do not wrap the program in an async IIFE: a return inside another function does not reach the host. In agent mode a successful program returns exactly `{to: "model", facts: {...}}` or `{to: "user", message: "..."}`. Direct `terrarium run` accepts any JSON-compatible return value.

## Cross-run state

- Filesystem effects persist across runs; JavaScript state does not. Program-provided data enters the next model context only through agent `facts`.
- Agent `facts` must serialize to at most 16384 bytes. Keep only decision-relevant paths, counts, statuses, and bounded samples. Do not return complete scan results, whole file contents, large arrays, credentials, or unbounded exception text. The direct result limit is 24 KiB; both are hard boundaries, not targets.
- If large data must survive the run, write it to an authorized file and return only its path, count, and short summary. A path in facts is a reference: the next step must call a host API to use that file.
- The host adds bounded status, errors, elapsedMs, and host-derived write receipts to model observations. Do not duplicate them; receipts describe committed writes even when a later operation fails. Use elapsedMs to avoid repeating unnecessarily expensive operations.

## Closed-loop edit shape

When the request mechanically defines candidate scope and the replacement rule, keep discovery, action, and verification in one run:

```run
const root = "/path/from-the-environment"; // replace with an authorized root from the environment
const oldText = "OLD_TOKEN";
const newText = "NEW_TOKEN";
const files = new Set();
let oldCount = 0;
let applied = 0;
let stage = "discover";
let currentPath = null;
try {
  for await (const line of host.fs.scan(root, {contains: oldText})) {
    if (!line.text.includes(oldText)) continue; // the final JavaScript predicate belongs here
    files.add(line.file);
    oldCount += line.text.split(oldText).length - 1;
  }
  if (oldCount === 0) {
    return {to: "model", facts: {phase: "no_match", oldText}};
  }
  stage = "apply";
  for (const path of files) {
    currentPath = path;
    // all: true only when the task rule covers every occurrence in this file
    applied += host.fs.replace(path, oldText, newText, {all: true}).replacements;
  }
  currentPath = null;
  stage = "verify";
  let residual = 0;
  for await (const line of host.fs.scan(root, {contains: oldText})) {
    if (line.text.includes(oldText)) residual += line.text.split(oldText).length - 1;
  }
  if (residual === 0 && applied === oldCount) {
    return {to: "user", message: `Updated ${files.size} files with ${applied} exact replacements.`};
  }
  return {to: "model", facts: {phase: "postcondition_failed", oldCount, applied, residual}};
} catch (error) {
  const message = error && error.message ? error.message : String(error);
  return {to: "model", facts: {phase: "operation_failed", stage, currentPath, applied,
    error: {kind: "filesystem_operation_failed", message: message.slice(0, 240)}}};
}
```

Adapt paths, predicates, exclusions, no-match meaning, and the postcondition to the request. If candidate scope needs semantic interpretation that cannot be encoded safely, return bounded evidence before changing state; otherwise do not turn discovery results into a model checkpoint.

## Filesystem selection

- Use only authorized roots listed in the environment. A denied path is final for this invocation; do not retry alternate spellings or invent a mount.
- `host.fs.list` inspects one directory level. `host.fs.walk` yields one `{file, size}` entry per regular file, for recursive counts, size totals, and path lists. `host.fs.scan` yields one `{file, no, text}` line, for recursive content search. Counting scan yields counts lines, not files. For one known file, use `host.fs.read` or `host.fs.text` instead. `read` is the display channel with stable `N: text` line numbers; never add or parse those prefixes yourself. `text` is the plain whole-file channel for programmatic transformation.
- Walk and scan defaults make routine scope exclusions deterministic: .gitignore is respected and hidden entries are skipped, so build, dependency, and version-control trees (target, node_modules, .git, and similar) are out of scope without a listing step. Pass `skipDirs` only for extra prunes beyond the defaults; do not list a directory to judge routine scope.
- `host.fs.scan` may use `contains` as a Rust-side literal prefilter when that literal is required by every relevant match. JavaScript remains the final predicate for regexes, case rules, multiple conditions, cross-line state, and custom limits.
- `host.fs.replace` is for exact changes. It fails instead of guessing, requires one match by default, and permits `{all: true}` only when every exact occurrence in that file should change. `host.fs.write` is for new files, complete rewrites, or non-exact transformations. Writes require a writable mount; verify the requested semantic postcondition rather than re-reading solely to confirm a write.

Available APIs and current mounts:

{{HOST_API}}

{{MOUNTS}}
</tool_contract>
