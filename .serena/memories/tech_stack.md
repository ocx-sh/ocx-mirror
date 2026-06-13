# Tech Stack

- Rust **edition 2024**, toolchain pinned `rust-toolchain.toml` channel **1.95.0** (rustfmt+clippy). Keep in sync with `external/ocx/rust-toolchain.toml` on submodule bumps.
- Task runner: [`task`](https://taskfile.dev) — config `taskfile.yml` + `taskfiles/`, `.taskrc.yml`.
- Toolchain/env via direnv + `ocx direnv export` (`ocx.toml`).
- Docs: mkdocs-material (`docs/`, `mkdocs.yml`) → GitHub Pages.
- Acceptance harness: pytest under `test/`, driven by `uv`, Docker registry on `:5000`.
- Remote: `git@github.com:ocx-sh/ocx-mirror.git`.

## Dependency model (CRITICAL — read CLAUDE.md "Dependency model" before touching Cargo.toml)
- `ocx_lib = { path = "external/ocx/crates/ocx_lib" }` — **git submodule**, NOT a published crate. Bumping ocx_lib = bumping the submodule pointer (procedure in README.md).
- `[patch.crates-io]` re-declares ocx's fork patches into the **nested** submodules (`external/ocx/external/...`). Patches don't travel with path deps; dropping the table silently resolves unpatched crates.io releases. CI asserts fork source via `cargo tree -i oci-client`.
- Dep feature lists copied verbatim from ocx `[workspace.dependencies]` — keep in sync on submodule bumps.
- Always clone/checkout `--recurse-submodules`.