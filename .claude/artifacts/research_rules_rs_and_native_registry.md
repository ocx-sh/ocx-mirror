# Research: rules_rs vs rules_rust; OCI registry on Docker-less macOS/Windows runners

## Metadata

**Date:** 2026-11-13
**Domain:** build-systems, ci-cd
**Triggered by:** `/hex-discuss` entry prior-art scan for `.agents/discussions/ocx-ci-parity.md`
**Expires:** 2027-01-13 (rules_rs releases weekly; re-check sooner)
**Axes:** prior art / vendor docs

## Key findings

### A. rules_rs vs rules_rust

- rules_rs 0.2.4 on BCR (2026-11-12); 8 releases in 7 weeks, pre-1.0. [BCR](https://registry.bazel.build/modules/rules_rs), [hermeticbuild/rules_rs](https://github.com/hermeticbuild/rules_rs) (~116 stars, 35 open issues).
- Not a replacement: ships a pinned, patched rules_rust (Windows linking, rust-analyzer) and re-exports rules via `@rules_rs//rs:*`; `patch`/`override_repo` hook.
- Crates: `crates.from_cargo` reads `Cargo.lock` directly — no splicing, no `Cargo.bazel.lock.json`; git-dep checksums in `MODULE.bazel.lock`. No `crate.spec`, no vendoring. Self-referencing dev-deps can broaden features.
- Toolchains: depends on BCR `llvm`; targets Linux GNU/musl, Windows MSVC/GNU/GNULVM; toolchains from hermeticbuild/rust-redist (zstd), `use_rust_redist = False` opt-out. Darwin not called out in README.
- Windows host: explicit host platform needed for ABI; default exec toolchain MSVC-flavoured; GNULVM may need `libunwind`.
- Remote cache not discussed in README; clippy aspect not documented; `lint_config` from `[lints]`.
- Migration: compatibility mode keeps rules_rust toolchains; existing `@rules_rust` loads keep working. Selenium switched (rationale unread). Root aliases / `data.bzl` / `defs.bzl` removed after 2027-03-01.
- rules_rust 0.75.0 (2026-10-26): Cargo toolchain, dlltool, lint/doctest/coverage on test exec platform, C++ toolchain via rules_cc, Windows proc-macro-srv, no libresolv on Darwin. Incumbent, `crate_universe` splicing (current mirror mechanism).

### B. Registry on macOS/Windows runners

- zot v2.1.23 (2026-11-02) ships darwin-arm64/amd64 and windows-amd64/arm64 binaries (+ `-minimal`, checksums). [releases](https://api.github.com/repos/project-zot/zot/releases/latest)
- zot runs `zot serve config.json`; GC on by default (`gcDelay` 1h), dedupe on (hard links). [storage docs](https://zotregistry.dev/v2.1.0/articles/storage/)
- zot runs the OCI conformance suite in CI (linux-amd64 only). [conformance](https://conformance.opencontainers.org/static/v1.1/instructions/zot/)
- distribution v3.1.2 ships Linux tarballs only; darwin/windows would need a source build (unverified).

## negative

- All rules_rs Windows/darwin/clippy claims are vendor-side; issue tracker unread; README examples lag BCR (0.1.3 vs 0.2.4).
- No zot Windows quirk reports found (incl. NTFS hardlink dedupe) — absence of evidence.
- No source on zot vs registry:2 for manifest/tag deletion, referrers, unreferenced manifests, mediaType strictness. Tests likely want `gc: false`.

## leads

- rules_rs issues: Windows host, darwin, clippy; `rust_clippy`/`rustfmt` aspect wrappers.
- `use_rust_redist` zstd archives vs remote-cache key stability.
- zot delete semantics (needs `delete` support / access-control) vs `REGISTRY_STORAGE_DELETE_ENABLED`.
- Cheapest conformance probe: run the mirror acceptance suite against a `gc: false` zot on Linux first.
