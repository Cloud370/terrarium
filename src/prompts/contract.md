<tool_contract>
Execution:
- The host recognizes only one complete ES2020 JavaScript program in each response.
- Put that program in one standalone ` ```run ` block. The opening line must be exactly ` ```run ` and the closing line must be exactly ` ``` `.
- ` ```javascript ` and every other fence are display-only; they are never executed.
- Each run starts with a fresh JavaScript environment. Top-level `await`, ordinary functions, and `try/catch` are available. There are no nested run, model-call, or sub-agent APIs.
- A normal `return` ends this run and sends its value to the next response. `host.agent.answer(text)` submits the final answer and ends the session.

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
    host.agent.answer(
      `No ${needle} matches under ${root}; scanned ${entries.length} top-level entries.`
    );
  } else {
    const first = matches[0];
    const context = await host.fs.read(
      first.file,
      Math.max(1, first.line - 2),
      first.line + 2
    );
    host.agent.answer(
      `First matches: ${JSON.stringify(matches)}\n\nContext:\n${context}`
    );
  }
} catch (error) {
  const message = error && error.message ? error.message : String(error);
  host.agent.answer(`Could not complete the inspection: ${message}`);
}
```

The example is one complete attempt: it confirms the scope, searches efficiently, reads only useful context, handles empty and error paths, and answers when it can. Adapt the root, glob, predicate, and final evidence to the user's request. Do not copy an example path that is not listed in the environment.

The first non-blank program line may be `// timeout-ms: N`. The default and hard maximum are stated in the main instructions. A non-final run result contains `ok`, `value`, `stdout`, `error`, `termination`, `timedOut`, and `elapsedMs`. Use errors and timeouts to correct or narrow the next operation; use `elapsedMs` to avoid unnecessarily expensive operations.

Filesystem:
- Use only the virtual roots listed in the environment.
- `host.fs.list(dir)` returns sorted objects `{name, type, size}` for one directory level. `type` is `file`, `directory`, `symlink`, or `other`; `size` is bytes for regular files and `null` otherwise. Inspect fields directly; never parse display text.
- For recursive counts or totals, list one directory at a time, recurse only into `directory`, add sizes only for `file`, record traversal errors, and report the result as incomplete if required scope could not be listed.
- `host.fs.read` is for bounded line windows. `host.fs.text` is for program-side whole-file text. `host.fs.scan` is for bounded tree searches. Choose the smallest operation that answers the request.
- `host.fs.write` is allowed only below a writable mount named in the environment. A denied write is final for this invocation; do not retry through another path or spelling.

The available API signatures and current mounts follow.

{{HOST_API}}

{{MOUNTS}}
</tool_contract>
