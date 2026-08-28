# Terrarium documentation

Maintained specifications live in this directory. They are the current project contract and should be updated in place when behavior changes.

- [`design.md`](design.md) - approved mechanism direction and explicit non-goals
- [`protocol.md`](protocol.md) - current wire and execution protocol
- [`configuration.md`](configuration.md) - current environment configuration and planned config-file shape
- [`security.md`](security.md) - trust boundary and isolation claims
- [`web-ui.md`](web-ui.md) - integration boundary for a future Web UI or service
- Runtime prompts are not documentation. They live in `src/prompts/` because changing them changes agent behavior. The JavaScript runtime prelude lives in `src/runtime/`.
