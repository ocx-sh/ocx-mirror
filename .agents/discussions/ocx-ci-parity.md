# Discussion: Bring ocx-mirror's CI to parity with ocx (conventions, rules_rs, readiness, zot)

State: handed-off → loop · Updated: 2026-11-13
Ratified: 2026-11-13 → loop
Confidence: ratified by owner (explicit yes, 2026-11-13); research vintages 2026-11-13 (codebase recon vs ocx v0.6.6, prior-art scan rules_rs 0.2.4 / zot 2.1.23)
Participants: owner + hex-discuss; 2026-11-13

## Intent

ocx-mirror was split out of the ocx mono-repo and is meant to track ocx's build and test conventions. ocx (`/home/mherwig/dev/ocx`, release v0.6.6) has since moved on and the mirror lags on four points:

1. the ocx-sh/conventions copier baseline (mirror `v0.3.2`, ocx `v0.3.4`);
2. the Bazel Rust rules (mirror: `rules_rust` 0.74.0 patched, `hermetic_cc_toolchain`, `apple_support`, `crate_universe` with `Cargo.bazel.lock.json`; ocx: `rules_rs` + BCR `llvm`, `Cargo.lock` read directly);
3. test tiering (ocx has `release-readiness.yml` + `readiness-gate.yml`; the mirror's `release.yml` is gated only by `basic-repo.yml`, and its `deep-repo.yml` runs no acceptance);
4. native acceptance on macOS and Windows (ocx runs a per-session zot from an ocx package with no Docker; the mirror's harness needs docker-compose `registry:2`).

Goal: the mirror matches ocx on all four points, taking ocx's files as the template wherever the mirror has no reason to differ.

## Requirements

### Scope by PR (files touched)

1. **Conventions bump.** `.copier-answers.yml` (`_commit`) and every template-owned file: `taskfiles/baseline.taskfile.yml`, `.pre-commit-config.yaml`, `cliff.toml`, `renovate.json`, `CLAUDE.md`, `.gitleaks.toml`, `.github/zizmor.yml`, `.github/workflows/verify-{basic,deep}.yml`, `bazel/baseline.bazelrc`. Applied with `task copier:update` only, never hand-edited.
2. **zot harness.**
   - Harness: `test/conftest.py`, a new `test/src/zot.py` (or the mirror's equivalent location) ported from `/home/mherwig/dev/ocx/test/src/zot.py` with `gc: false`, plus `test/docker-compose.yml`, which shrinks to the Sigstore stack and any `registry:2`-only fixture.
   - Pins and Bazel: `ocx.toml` and `ocx.lock` (zot package pin, same version as ocx), `test/bazel_accept.sh`, `test/BUILD.bazel`.
   - Workflows: `.github/workflows/acceptance.yml` gains the `no-docker`, `limits-dir` and `release-binary` inputs ocx has. `.github/workflows/deep-repo.yml` gets an `acceptance-native` job on macOS and Windows.
   - Task and docs: `test/taskfile.yml` (Windows `cmd.exe` path, as in ocx), and docs for the env vars (`REGISTRY`, `MIRROR_REGISTRY`, `ZOT_REGISTRY` → per-session ports).
3. **rules_rs.**
   - Bazel module: `MODULE.bazel` (`rules_rs`, `llvm`, toolchains extension, `crate.from_cargo` on `Cargo.lock`, gnullvm triples, the osx frameworks list from `build/hermetic-llvm`), `.bazelrc`.
   - BUILD files: `BUILD.bazel`, `crates/*/BUILD.bazel`, `test/BUILD.bazel`, `docs/BUILD.bazel` (only if load paths change).
   - Release: `release/` (delete both `rules_rust_windows_*.patch`; `provenance.bzl` load paths).
   - Gate scripts: `scripts/bazel_build_drift.py`, `scripts/bazel_test_floor.py`, `crates/TEST_TARGET_MAP.toml`.
   - Taskfiles: `taskfiles/bazel*.yml` (`bazel:bootstrap`, `bazel:patch:check` lose the `Cargo.bazel.lock.json` leg, `bazel:build:release` gets gnullvm), plus `.gitignore`.
   - Build matrix: `build-matrix.yml` (Windows runner/target).
   - Docs: the `AGENTS.md` Layout table, the "Dependency model" and "Bazel loop" sections, and `.agents/memory/hex.md` notes that name `Cargo.bazel.lock.json`.
   - Cleanup: removing the `build/hermetic-llvm` branch and its worktree.
4. **Release readiness.** New `.github/workflows/release-readiness.yml`, `.github/workflows/readiness-gate.yml` and `scripts/release_readiness_gate.py` (with `--self-test`, per the `scripts/` convention). Also edits to `.github/workflows/release.yml`, the release taskfile (tag refusal), `README.md` (release procedure) and `AGENTS.md` (CI tier table).

### Out of scope

- Adopting cargo-dist, publishing changes, and any change to the `Deploy Dev` / release registries.
- ocx-only lanes: `shell-activation*`, shims, TypeScript, sdkgen, docker-publish.
- Bumping the `external/ocx` submodule. Use `/update-ocx` separately.
- Changing the nightly cadence or making deep tiers a per-PR gate.
- Moving macOS or Windows acceptance into the basic tier.

## Decisions

- **Windows release target → `windows-gnullvm`**, matching ocx. This drops the two rules_rust Windows patches (`release/rules_rust_windows_path_deps.patch`, `release/rules_rust_windows_ldeps.patch`) and the `OCX_MIRROR_CLANG_CL` aws-lc workaround. The mirror is a standalone CLI, so no consumer depends on the ABI. Counter-argument, noted: the release asset changes ABI in place, so release notes must say so.
- **`build/hermetic-llvm` is superseded by rules_rs.** It is not landed. Its `osx.frameworks` list (copied from ocx) is reused. After the rules_rs work lands, the branch and `.agents/worktrees/hermetic-llvm` are removed.
- **zot is the acceptance registry on every OS, like ocx.** Port ocx's per-session launcher (`/home/mherwig/dev/ocx/test/src/zot.py`) with `gc: false`, at kernel-chosen ports, obtained as an ocx package (`ocx.sh/project-zot/zot`). Docker stays only for the Sigstore stack and any test that needs `registry:2`; those skip under a no-docker switch. Linux basic runs zot per PR. macOS and Windows run it in the deep/nightly tier and in release readiness only.
- **Four PRs, in this order:** (1) conventions bump → (2) zot harness and native acceptance → (3) rules_rs + llvm + gnullvm → (4) release readiness. Readiness goes last so that it gates the final release targets and the finished harness. Each PR lands and is green before the next one starts.
- **Release readiness is copied from ocx:**
  - `release-readiness.yml` runs on `workflow_dispatch` only, with `bypass` and `reason` inputs. It runs the release build matrix with nothing published, then native acceptance on Linux, macOS and Windows against the release binaries, then a verdict.
  - `readiness-gate.yml` is called from `release.yml` and runs `scripts/release_readiness_gate.py --sha`, which requires a green or bypass run for the tagged SHA.
  - `task release:tag` (or `release:prepare`'s tag step) refuses without one.
  - ocx-only parts are dropped: cargo-dist `dist plan`, `shell-activation-deep`, and shim and schema staging.

## Research

- `.claude/artifacts/research_ocx_ci_parity_gap.md` — entry recon (2026-11-13): four-axis gap vs ocx v0.6.6. Notable: `build/hermetic-llvm` is llvm-on-rules_rust, not rules_rs; ocx ships Windows as gnullvm, mirror as msvc; mirror deep tier has no acceptance job; zot `gc: false` already handles the patch-eviction test's untagged-manifest need.
- `.claude/artifacts/research_rules_rs_and_native_registry.md` — prior-art scan (2026-11-13): rules_rs 0.2.4 is a patched-rules_rust wrapper reading `Cargo.lock` directly (drops `Cargo.bazel.lock.json` + the mirror's two Windows patches), pre-1.0 and fast-moving; zot ships native darwin/windows binaries, `registry:2`/distribution does not.

## Related

- ocx reference files (read-only, `/home/mherwig/dev/ocx`): `MODULE.bazel`, `.github/workflows/{release-readiness,readiness-gate,deep-repo,acceptance,basic-repo}.yml`, `scripts/release_readiness_gate.py`, `test/src/zot.py`, `test/conftest.py`, `test/taskfile.yml`, `test/bazel.bzl`, `ocx.toml`.
- Unlanded branch `build/hermetic-llvm` (worktree `.agents/worktrees/hermetic-llvm`) — superseded; source of the `osx.frameworks` list.
- [ocx-sh/conventions](https://github.com/ocx-sh/conventions) — copier baseline.
- Prior baseline adoption on main: [24d5edb](https://github.com/ocx-sh/ocx-mirror/commit/24d5edb).

## Open questions

- [NEEDS CLARIFICATION: Which conventions version is the target, ocx's `v0.3.4` or the latest tag when PR 1 runs?]
  Recommended: the latest tag at execution time, at least `v0.3.4` — the baseline only moves forward, and `task baseline:check` pins it.
- [NEEDS CLARIFICATION: `rules_rs` / `llvm` / Rust toolchain versions — ocx's pins (`rules_rs` 0.2.2, `llvm` 0.8.25, Rust 1.95.0) or BCR latest (`rules_rs` 0.2.4)?]
  Recommended: ocx's pins exactly — the copy-exactly policy for shared dependencies, and ocx has them green on all release targets.
- [NEEDS CLARIFICATION: Shard count for native and readiness acceptance — ocx uses 4, the mirror's basic tier uses 1.]
  Recommended: start at the smallest count that keeps the slowest native shard under ocx's per-shard wall time. Measure on the first deep run and do not copy 4 blindly.
- [NEEDS CLARIFICATION: `.copier-answers.yml` `deep_os` — the mirror lists ubuntu+macos+windows, while ocx lists ubuntu only and runs native acceptance through `deep-repo.yml`.]
  Recommended: follow ocx's split if the template's deep stub adds nothing on macOS or Windows. Decide in PR 1 from what the v0.3.4 template generates.
- Which mirror tests other than `test/tests/test_mirror_patch.py:359` assume `registry:2` semantics (deletion, referrers, unreferenced manifests)? The first Linux zot run answers this. Each failure gets either a zot config fix or a docker-only skip with a stated reason, never a silent skip.

## Verification

- **PR 1:** `task baseline:check` is green, then `task verify:scoped`, and CI `basic-gate` is green.
- **PR 2:**
  - Linux: the acceptance suite is green against per-session zot both locally (`task test:parallel`) and in CI.
  - Prove zot actually served it: with no compose `registry:2` up, assert the registry port belongs to the launched zot process.
  - The `SUITE_FLOOR` / `SKIP_CEILING` / `XFAIL_CEILING` floors in `test/` do not drop. Any new no-docker skip is counted and named.
  - Windows is reproduced locally first via cross-compile + staging on `C:` (owner's WSL setup), then a dispatched `verify-deep` run with `acceptance-native` is green on macOS and Windows.
- **PR 3:**
  - `task bazel:test:unit`, `bazel:test:accept`, `bazel:build:drift`, `bazel:lint:clippy` and `bazel:lint:fmt` are green, and the `TEST_TARGET_MAP.toml` counts do not drop.
  - `task bazel:build:release TARGET=<triple>` builds for every release triple, including `x86_64-pc-windows-gnullvm` and `aarch64-pc-windows-gnullvm`.
  - The Windows binary runs `ocx-mirror --json version` on the Windows host.
  - A dispatched deep run (release build matrix) is green, and the remote cache gets hits on a second run.
- **PR 4:**
  - `scripts/release_readiness_gate.py --self-test` passes.
  - Show both outcomes: the gate refuses a SHA with no readiness run (red), and a dispatched readiness run on the branch head lets the gate pass for that SHA (green).
  - The release workflow is never tagged or run for real. Exercise the gate with dispatch only.
