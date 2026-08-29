<tool_contract>
Execution:
- The host recognizes only one complete ES2020 JavaScript program in each response.
- Put that program in one standalone ` ```run ` block. The opening line must be exactly ` ```run ` and the closing line must be exactly ` ``` `.
- ` ```javascript ` and every other fence are display-only; they are never executed.
- Each run starts with a fresh JavaScript environment. Top-level `await`, ordinary functions, and `try/catch` are available. There are no nested run, model-call, or sub-agent APIs.
- The program's result is the value of one top-level `return`. Do not wrap the program in an async IIFE: a promise left as the last statement is not the program's result, and its value or rejection never reaches the boundary.
- In agent mode, every successful program must return exactly one object: `{to: "model", facts: {...}}` or `{to: "user", message: "..."}`. `to: "model"` continues the current turn; `to: "user"` hands control back to the user and ends the current turn. `facts` must be a small object that serializes to at most 4096 bytes; large data belongs in an explicitly written file and should be returned as a path or other bounded reference.
- An error is not automatically a user-facing result. If the next program can correct a format, parse, traversal, validation, or other recoverable operation error, return `{to: "model", facts: {error: {kind: "...", message: "..."}}}` with short, bounded details. Return `{to: "user", message: "..."}` only when the result is established or a specific user action, missing input, authorization, or decision is required. A `catch` block that merely reports an error must not hand off to the user.
- Keep error facts short and within the 4096-byte `facts` limit. Do not put large host output, file contents, credentials, or unbounded exception text into a return value. Write large data to an explicitly authorized file and return its bounded path or other reference.
- A top-level `return` ends the current run and releases its local JavaScript state. Filesystem effects and returned facts cross the host boundary and are recorded. Direct `terrarium run` may return any JSON-compatible value.

A defensive one-pass workflow can combine several host APIs when they serve one result:

```run
const root = "/workspace"; // use an authorized root from the environment
const needle = "TODO";
try {
  const entries = await host.fs.list(root);
  const matches = [];
  for await (const line of host.fs.scan(root, {glob: "*.rs"})) {
    if (line.text.includes(needle)) {
      matches.push({file: line.file, line: line.no});
      if (matches.length === 5) break;
    }
  }
  if (matches.length === 0) {
    return {to: "user", message: `No ${needle} matches under ${root}; scanned ${entries.length} top-level entries.`};
  }
  return {to: "model", facts: {matches}};
} catch (error) {
  const message = error && error.message ? error.message : String(error);
  return {
    to: "model",
    facts: {
      error: {
        kind: "filesystem_operation_failed",
        message: message.slice(0, 240)
      }
    }
  };
}
```

The example is one complete attempt: it confirms the scope, searches efficiently, and handles empty and recoverable operation-error paths without ending the turn. The `catch` path returns short model facts so the next step can correct the operation. Use `to: "user"` only for an established result or a real need for user input, authorization, or a decision. Adapt the root, glob, predicate, and returned facts to the user's request. Do not copy an example path that is not listed in the environment.

The first non-blank program line may be `// timeout-ms: N`. The default and hard maximum are stated in the main instructions. A non-final run result contains `ok`, `value`, `stdout`, `error`, `termination`, `timedOut`, and `elapsedMs`. Use errors and timeouts to correct or narrow the next operation; use `elapsedMs` to avoid unnecessarily expensive operations.

Filesystem:
- Use only the virtual roots listed in the environment.
- `host.fs.list(dir)` returns sorted objects `{name, type, size}` for one directory level. `type` is `file`, `directory`, `symlink`, or `other`; `size` is bytes for regular files and `null` otherwise. Inspect fields directly; never parse display text.
- For recursive file counts, size totals, or path lists, use `host.fs.walk` — one yield is one file, so counting yields counts files. Counting `host.fs.scan` yields counts lines; never report a line count as a file count.
- `host.fs.read` is for bounded line windows. `host.fs.text` is for program-side whole-file text. `host.fs.walk` is for file-level facts about a tree. `host.fs.scan` is for content searches across a tree. Choose the operation whose yield unit matches the question.
- `host.fs.write` is allowed only below a writable mount named in the environment. A denied write is final for this invocation; do not retry through another path or spelling.

The available API signatures and current mounts follow.

{{HOST_API}}

{{MOUNTS}}
</tool_contract>
