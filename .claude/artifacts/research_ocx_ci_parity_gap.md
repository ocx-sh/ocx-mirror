# Research: ocx-mirror vs ocx CI parity gap (conventions, rules_rs, readiness, zot)

## Metadata

**Date:** 2026-11-13
**Domain:** ci-cd
**Triggered by:** `/hex-discuss` entry recon for `.agents/discussions/ocx-ci-parity.md` — move ocx-mirror onto ocx's current conventions baseline, rules_rs, release-readiness tier and zot-based native acceptance
**Expires:** 2027-02-13
**Axes:** codebase recon (read-only comparison of `/home/mherwig/dev/ocx` at v0.6.6 against this repo at `6b5ade0`)

## Key findings

### 1. Conventions baseline (mirror v0.3.2, ocx v0.3.4)

- `.copier-answers.yml` deltas: `_commit`; `deep_os` (mirror ubuntu+macos+windows, ocx ubuntu only); `repo_workflows.secrets` (mirror adds `BAZEL_CACHE_READ_AUTH`, `OTEL_OTLP_AUTH`); `zizmor_ignore` (ocx lists `release.yml`). No new answer key evident between the two versions (inferred, template not read).
- Template-owned files differ by line count: `taskfiles/baseline.taskfile.yml` 246/258, `verify-deep.yml` 106/102, `verify-basic.yml` 158/156. `.github/zizmor.yml` 14/134 and `.gitleaks.toml` 18/66 differ far beyond a patch bump — likely per-repo answers, unverified.

### 2. Bazel Rust rules

- ocx `MODULE.bazel`: `rules_rs` 0.2.2 + BCR `llvm` 0.8.25; toolchains extension pins Rust 1.95.0; `rules_rust` extension via `@rules_rs//rs:rules_rust.bzl`, unpatched; `crate.from_cargo` reads `Cargo.toml`/`Cargo.lock` directly — no `Cargo.bazel.lock.json`, no patches, `generate_lint_config = True`; eight `platform_triples`, Windows as **gnullvm**; only two annotations (`aws-lc-sys`, `zstd-sys`, `gen_build_script = "on"`).
- Mirror `MODULE.bazel`: `rules_rust` 0.74.0 with two Windows patches (`release/rules_rust_windows_path_deps.patch`, `release/rules_rust_windows_ldeps.patch`) via `single_version_override`; `hermetic_cc_toolchain` 4.3.0 (zig) + `apple_support` 1.24.2; `crate_universe` with gitignored `Cargo.bazel.lock.json`; `annotation_select` for octocrab (`CARGO_HOME`) and aws-lc-sys (`clang-cl`); `use_repo(crate, "crates__schemars-1.2.3")` — versioned spoke name, breaks on bump.
- Unlanded `build/hermetic-llvm` (worktree `.agents/worktrees/hermetic-llvm`): llvm on **rules_rust** only — drops zig/apple_support, keeps rules_rust patches, crate_universe lockfile, Windows **msvc** cross toolchain, `OCX_MIRROR_CLANG_CL`. Possible duplicate `repository_set` name `rust_linux_x86_64_musl` reused for aarch64 (MODULE.bazel lines 125-133, unconfirmed).
- Other rules_rust-shaped surface (not opened): every `BUILD.bazel`, `scripts/bazel_build_drift.py`, `scripts/bazel_test_floor.py`, `crates/TEST_TARGET_MAP.toml`, `taskfiles/bazel*.yml`, `release/provenance.bzl`.

### 3. Test tiering

- ocx `release-readiness.yml`: `workflow_dispatch` only (`bypass`, `reason` inputs). Builds every release target like `release.yml`, runs `scan-binaries.yml`, stages binaries natively on six platforms, runs `acceptance.yml` per platform (`shard-count: 4`, per-OS `limits-dir`, `release-binary: true`), shell-activation-deep, verdict. Publishes nothing.
- ocx `readiness-gate.yml`: called from `release.yml`; `scripts/release_readiness_gate.py --sha` requires a green (or bypass) readiness run for the tagged SHA; `task release:tag` refuses likewise.
- ocx `deep-repo.yml`: `acceptance-native` job on macOS + Windows, `shard-count: 4`, `no-docker: true`; `verify-deep.yml` nightly cron.
- Mirror: `basic-repo.yml` shard-count 1; `deep-repo.yml` has no acceptance job; no readiness workflow, gate script, `test/limits/<os>`, or `no-docker` input.

### 4. zot

- ocx gets zot as an ocx package `ocx.sh/project-zot/zot:2.1.21` (`ocx.toml`); Bazel `@tools//:zot`; harness `test/src/zot.py` starts primary + target registries per session on kernel-chosen ports, watchdog kills orphans; `registry:2` survives only for docker-only fixtures that skip under `OCX_TEST_NO_DOCKER=1` (`test/conftest.py:170`). Windows runs pytest via `cmd.exe` under `ocx exec zot` (`test/taskfile.yml:367`).
- Mirror harness: `test/conftest.py:64-69` brings up `registry:2` :5001, `registry:2` :5002, zot :5011 (referrers/signing only), plus Sigstore via `docker compose`; `test/bazel_accept.sh` needs a Docker daemon. No no-docker mode, no per-session zot launcher.
- `test/tests/test_mirror_patch.py:359` (`test_patch_evicts_nothing_a_consumer_could_have_pinned`) needs untagged manifests to survive. Mirror compose zot already sets `gc: false` (`test/docker-compose.yml:29`); the failure in `.agents/memory/hex.md` was default-GC zot from the sibling ocx compose, not an inherent zot limit.

## negative

- Line counts only for the baseline diff; equal counts unproven identical.
- No test run against a `gc: false` per-session zot.
- Windows ABI: ocx ships `windows-gnullvm`, mirror ships `windows-msvc` — not interchangeable; release artifact names/ABI change if mirror follows ocx.

## leads

- Read `/home/mherwig/dev/ocx/test/src/zot.py` `zot_config` for GC settings.
- Read `/home/mherwig/dev/ocx/test/conftest.py:62-170` for `_wants_zot` / no-docker skip logic.
- Inventory which other mirror tests assume `registry:2` semantics.
- Read `/home/mherwig/dev/ocx/scripts/release_readiness_gate.py`; map mirror `release.yml` (no cargo-dist) onto it.
