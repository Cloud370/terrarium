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
- The agent contract (`src/CONTRACT.md`) and role template (`src/MAIN.md`) are compiled into the binary via `include_str!` — treat them as code: changing them changes runtime behavior.
- `src/registry.rs` is the single source of the host API surface: `host.help()` and the contract are generated from it, so they cannot drift. Add new host capabilities there.
- By submitting a PR you agree your contributions are licensed under the [MIT license](LICENSE).
