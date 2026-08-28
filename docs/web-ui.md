# Web UI Integration Boundary

There is no Web UI or HTTP service in the current package. The command-line binary is not a service protocol: a future UI should not spawn it, scrape stderr, or infer state from exit codes.

## Current library boundary

The reusable library currently exposes:

- `Kernel::run` for one fresh QuickJS execution;
- `Kernel::contract` for the generated host contract;
- validated `Mount` values and `Kernel::new` for operator-declared filesystem scope;
- structured `Outcome` values for run results.

The CLI and outer agent loop remain adapters. The agent loop still owns model conversation state and terminal output; it has not yet been promoted to a typed session API.

## Future service direction

When a concrete Web UI consumer exists, add a small service boundary over the library. A reasonable starting shape is:

```text
start_session(request) -> SessionHandle
SessionHandle.events() -> stream<SessionEvent>
SessionHandle.cancel()
SessionHandle.result() -> Answer
```

That service would need to define session cancellation, event ownership, authentication, mount selection, and provider configuration before exposing them over HTTP or Server-Sent Events. It should use typed Rust calls and structured events rather than CLI text.

## Explicit non-goals for now

The current project does not implement:

- an HTTP/WebSocket/SSE server;
- a session event stream or JSONL trace;
- child-agent lifecycle events;
- artifact upload or download;
- browser-side provider configuration;
- browser-visible filesystem paths or provider credentials.

Provider credentials must remain in the eventual service process, and mounts must be selected by the operator or service policy rather than accepted as unrestricted browser paths.

Keep one Cargo package while only the CLI consumes the library. Split into a workspace only when a second service consumer actually exists.
