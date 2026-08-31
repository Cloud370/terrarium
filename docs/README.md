# Terrarium documentation

Maintained specifications live in this directory. They are the current project contract and should be updated in place when behavior changes.

- [`design.md`](design.md) - approved mechanism direction and explicit non-goals
- [`protocol.md`](protocol.md) - current wire and execution protocol, including the `access` block and authorization outcomes
- [`filesystem-authorization.md`](filesystem-authorization.md) - normative filesystem access and write preauthorization contract (Chinese: [`filesystem-authorization.zh-CN.md`](filesystem-authorization.zh-CN.md))
- [`configuration.md`](configuration.md) - current TOML profiles, credential references, and legacy fallback
- [`security.md`](security.md) - trust boundary and isolation claims
- [`web-ui.md`](web-ui.md) - integration boundary for a future Web UI or service
- [`model-profiles-and-durable-sessions.md`](model-profiles-and-durable-sessions.md) - implemented model profiles, protocol binding, JSONL session, and recovery contract
- Runtime prompts are not documentation. They live in `src/prompts/` because changing them changes agent behavior. The JavaScript runtime prelude lives in `src/runtime/`.
