# Contributing to terrarium

Thanks for your interest in contributing!

## Development setup

- Rust stable (1.87 or newer — the `rquickjs` dependency requires it)
- `mingw-w64` only if you cross-compile Windows binaries (`cargo build --release --target x86_64-pc-windows-gnu`)

## Everyday commands

```sh
cargo build --release        # build the binary
cargo test                   # run the test suite
cargo fmt --all              # format (CI enforces `--check`)
cargo clippy --all-targets -- -D warnings   # lint (CI enforces this too)
```

## Pull requests

- Keep PRs focused; one logical change per PR.
- CI must be green: `fmt --check`, `clippy -D warnings`, `test`, `build`.
- Runtime prompts (`src/prompts/`) and the JavaScript prelude (`src/runtime/prelude.js`) are compiled into the binary via `include_str!` — treat them as code: changing them changes runtime behavior.
- Maintained specifications live in `docs/` and should describe the current implementation or clearly label future behavior.
- `src/registry.rs` is the single source of the host API surface: `host.help()` and the prompt contract are generated from it. Add new host capabilities there.
- Keep presentation adapters thin: reusable behavior belongs in the library; `src/main.rs` and `src/cli.rs` handle process and terminal concerns.
- By submitting a PR you agree your contributions are licensed under the [MIT license](LICENSE).
