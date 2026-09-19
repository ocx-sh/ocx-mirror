# Tech Stack

- Rust **edition 2024**, toolchain pinned `rust-toolchain.toml` channel **1.95.0** (rustfmt+clippy). Keep in sync with `external/ocx/rust-toolchain.toml` on submodule bumps.
- Task runner: [`task`](https://taskfile.dev) — config `taskfile.yml` + `taskfiles/`, `.taskrc.yml`.
- Toolchain/env via direnv + `ocx direnv export` (`ocx.toml`).
- Docs: mkdocs-material (`docs/`, `mkdocs.yml`) → GitHub Pages.
- Acceptance harness: pytest under `test/`, driven by `uv`, Docker registry on `:5000`.
- Remote: `git@github.com:ocx-sh/ocx-mirror.git`.

## Dependency model (CRITICAL — read CLAUDE.md "Dependency model" before touching Cargo.toml)
- Ten `ocx_*` path rows into `external/ocx/crates/` — **git submodule**, NOT published crates. Upstream dissolved `ocx_lib` into responsibility-derived crates, so the surface is named per crate; bumping ocx is still bumping the submodule pointer (procedure in README.md).
- **No row for `ocx` (`crates/ocx_cli`)** — it is the CLI application; linking it dragged 160 packages (starlark host, LSP/DAP/REPL stack, gix family) in for two imports. Mirror-owned replacements: `src/tracing_init.rs` (verbatim copy, needs the `tracing-subscriber` row kept in sync with ocx) and `error::tls_exit_code`.
- `[patch.crates-io]` re-declares ocx's fork patches into the **nested** submodules (`external/ocx/external/...`). Patches don't travel with path deps; dropping the table silently resolves unpatched crates.io releases. CI asserts fork source via `cargo tree -i oci-client`.
- Dep feature lists copied verbatim from ocx `[workspace.dependencies]` — keep in sync on submodule bumps.
- Always clone/checkout `--recurse-submodules`.