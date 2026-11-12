# Research: Cargo native builds — CI portability and corporate cargo setup

## Metadata

**Date:** 2026-11-02
**Domain:** ci-cd
**Triggered by:** `.agents/discussions/cargo-native-build.md` (revisiting `adr_cargo_source_kind.md` for GitLab, configurable builds, corporate cargo)
**Expires:** 2027-05-02

## Direct Answer

Supplements `research_cargo_tooling.md` / `research_cargo_competitive_landscape.md` (2026-09) with the GitLab and corporate-environment angles they did not cover.

- **GitLab:** dynamic child pipelines (`trigger: include: artifact`) are the analogue of a JSON-driven GitHub matrix — limits 1000 downstream pipelines per hierarchy, 2 nesting levels, no CI variables in the dynamic include. `parallel:matrix` caps at 200 permutations. Hosted macOS runners: beta, Premium/Ultimate, gitlab.com only; hosted Windows: beta, all tiers, gitlab.com only, no Docker executor, ~5 min VM provisioning. **Self-managed GitLab has neither** — customers register their own runners. [docs.gitlab.com/ci/pipelines/downstream_pipelines, /ci/runners/hosted_runners/macos, /ee/ci/runners/hosted_runners/windows.html]
- **Corporate cargo:** proxy `http.proxy`/`CARGO_HTTP_PROXY` > git `http.proxy` > `HTTPS_PROXY`; CA `http.cainfo`/`CARGO_HTTP_CAINFO` (system certs otherwise), `http.proxy-cainfo`; mirrors via `[source.crates-io] replace-with` (`registry`, `local-registry`, `directory`, `git`); `net.offline`. `cargo install` ignores project-local config — only `$CARGO_HOME/config.toml` and env apply. [doc.rust-lang.org/cargo/reference/config.html]
- **Reproducibility:** `cargo install` ignores the packaged `Cargo.lock` unless `--locked`; crates published before Cargo 1.37 have no packaged lock; output varies with rustc, so pin toolchain, `--target`, `--profile`. [doc.rust-lang.org/cargo/commands/cargo-install.html]

## Cross-compile reach from one Linux host

| Tool | Reach | Caveat |
|---|---|---|
| cargo-zigbuild | Linux + macOS targets; glibc pin via `.2.17` suffix | macOS needs `SDKROOT` SDK; `zig cc -nostdinc`, compiler_rt gaps |
| cargo-xwin | Windows MSVC (cargo-dist uses it since 0.26) | page not fetched |
| cross | Linux, `*-windows-gnu` | no MSVC/Darwin images shipped |

## Prior art

- **cargo-dist:** native runner per target by default; cross (zigbuild/xwin) only when no native runner.
- **cargo-quickinstall:** central GH Actions farm, default features only, failed crates go to an exclude list, minisign-signed tarballs.
- **conda-forge:** recipe per package, `cargo auditable install --locked`, `cargo-bundle-licenses`.
- **mise `cargo:`/aqua `cargo`:** per-consumer `cargo install` fallback; mise defaults the quickinstall host off (trust) and notes prebuilt binaries ignore consumer cargo config.

## Gaps

quickinstall `build-version.sh` (native vs cross per target), cargo-xwin docs, nixpkgs `buildRustPackage`, Homebrew Rust conventions — not read.
