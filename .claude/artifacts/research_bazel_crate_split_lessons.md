# Research: ocx Bazel adoption lessons + ocx-mirror crate map

## Metadata

**Date:** 2026-09-22
**Domain:** ci-cd
**Triggered by:** `.agents/discussions/bazel-crate-split.md` — Bazel build, crate split and ocx promotion for ocx-mirror
**Expires:** 2027-03-22

## Direct Answer

ocx's Bazel adoption landed on `main` (lane swap `02ff88f0`), but its own measurement
(`../ocx/.claude/artifacts/measurement_bazel_r2.md` § VERDICT) recorded **NO-GO for the lane
swap on wall-clock grounds**: warm `bazel test //crates/...` 4.44 s vs nextest 71.01 s, a 66.6 s
win against a ≥240 s bar, because 58 % of `Smoke (Linux)` is `cargo nextest list` / `cargo build
--release`, which Bazel does not replace, and the 3-OS `verify-deep` matrix is gated by Windows.
The follow-up goal (`plan_test_speed_tiers.md`, branch `hex/test-speed-tiers`, plan-approved,
in flight) attacks cache **correctness** instead of hit rate.

## Key Findings — lessons to carry over

1. **Test-report regression** (`../ocx` commit `8a8546f8`). The lane swap deleted the consumers of
   `target/nextest/ci/junit.xml`. `bazel-testlogs/**/test.xml` has one `<testcase>` per *target*;
   the Rust test name appears only in `system-out` CDATA, so the PR publisher named 34 targets and
   no test. Fix: `bazel_test_floor.py --junit` rebuilds per-test JUnit from `test.log` (classname =
   label, name = libtest path, `<failure>` = panic block), floored so case count must equal the
   libtest summary. Plus `.bazelrc` `build:ci --remote_download_regex=.*/test\.(xml|log)$`, since
   `--remote_download_minimal` leaves no `test.xml` on disk for cached targets. rules_rust native
   JUnit is still experimental (rules_rust PR #4181, issue #1303).
2. **Provenance churn kills acceptance caching** (`adr_test_speed_tiers.md:37`). `ocx_cli/build.rs`
   bakes `describe`, commit time and `GITHUB_*` into the binary; every acceptance `sh_test` hashes
   `test/bin/ocx*`, so every commit re-runs all 181 targets (1421 s cold serial vs 160 s
   `task test:parallel`). Fix direction: stamp provenance only for release (vergen idempotent,
   Bazel stable/volatile status). ocx-mirror has no `build.rs` today — keep it that way, or stamp
   release-only.
3. **Acceptance verdicts are local-disk only.** No remote writer exists for acceptance results;
   every CI acceptance run starts cold. Caching pays on agent/dev machines only
   (`adr_test_speed_tiers.md:42`).
4. **Disk-cache false green:** Bazel 9.2.0 marks a `--disk_cache` hit `cachedRemotely=true` even
   with `--remote_cache=` empty — key assertions on `executionInfo.strategy`.
5. **`--remote_instance_name` inert over an HTTP cache**; needs URI path prefix +
   server-side AC key mangling. Remote cache host needs Basic auth on GET.
6. **`--local_test_jobs=1` is global** — use the `exclusive` tag on docker-backed tests instead.
7. **Submodule BUILD files:** `external/*` is mode 160000, so BUILD files for submodule crates
   cannot be committed in the parent (`3d445fb6` generates them). Directly relevant: ocx-mirror
   consumes `external/ocx` and its nested forks.
8. **Scoped test arm skipped reverse dependents** (`adr_test_speed_tiers.md:40`) — `bazel test
   //crates/<changed>:all` misses dependents; use rdeps query.
9. **Hand-written BUILD files**, not `gazelle_rust` (v0.1.0 only, no `[patch.crates-io]` story).
   Linker: rust-lld is default since Rust 1.90; `2a8c663d` still had to force lld.
10. **Win is bounded by what Bazel touches.** Bazel took only the Linux leg; the Windows/macOS legs
    and release builds stay cargo. Two build systems compile the same crates in T1.

## Cross-module Bazel (prior art, web)

- Each module's crate_universe creates its own hub; linking A's targets into B with a second hub
  gives two copies of shared crates → type mismatch across the A/B boundary. Cross-hub coalescing
  is unmerged work (hermeticbuild/rules_rs#255). Transitive crate_universe repos must ship a
  lockfile; repinning across module boundaries unsupported (rules_rust discussion #2879).
- `crate.spec(path=…)` cannot combine with `version`/`git`; patched-crate aliases can point at
  the vendored location instead of the patch (rules_rust#3732). No evidence of support for a
  `[patch.crates-io]` into a nested submodule.
- Docker-backed tests: `exclusive` + `requires-network`; linux-sandbox inside a container needs
  `--privileged`. Two public cargo→Bazel evaluations were negative (micromegas#1610;
  mmapped.blog gives no numbers).

## ocx crate tiers (from `../ocx/.claude/artifacts/system_design_crate_workspace.md`)

internal (satellite may never link) · ecosystem (lockstep submodule consumer may link:
`ocx_config`, `ocx_console`, `ocx_exit`, `ocx_index`, `ocx_oci`, `ocx_package`, `ocx_sign`,
`ocx_util`) · interface (`ocx_exit`). Enforced by ocx `task satellite:verify` in
`verify-deep.yml`. No existing ocx crate fits Python lock, generic HTTP download, GitHub-release
discovery or JUnit reporting — each would be a new ecosystem crate.

## ocx-mirror module map

- Mirror-specific: `src/command/` (~23.5K lines), `src/spec/` (~15.3K), `src/pipeline/`
  registry_sync, dist_sync, orchestrator, `command/package/pipeline/generate/*`, `error.rs`.
- Generic candidates: `crates/ocx_python` (~4K, already split), `src/source/*` (github_release,
  url_index, pypi, pylock, generator ~2.1K), `http.rs` + `auth.rs`, `junit.rs` + `run_summary.rs`,
  `discord.rs`, `tracing_init.rs`, `filter.rs`/`normalizer.rs`/`resolver.rs`, `annotations.rs`,
  pipeline `ocx_cli.rs`/`push.rs` (ocx subprocess boundary).
- Candidate boundaries: source, http+auth, ci-report (junit+run_summary), ocx_python. No cycles
  observed; `pipeline/target_registry.rs` placement is the precedent for avoiding upward edges.

## Acceptance suite ↔ binary

`test/conftest.py:130-156` resolves `OCX_MIRROR_COMMAND` / `OCX_COMMAND` env first, else
`test/bin/*`; `test/taskfile.yml:22-38` builds and copies. The suite couples only to argv, exit
codes and output — an older `test/` tree can run against a newer binary by env var.

## Sources

`../ocx/.claude/artifacts/{measurement_bazel_r2,decision_bazel_adoption,adr_test_speed_tiers,
plan_test_speed_tiers,system_design_crate_workspace}.md`, `../ocx/.agents/memory/hex.md`,
`../ocx` commits `8a8546f8`, `02ff88f0`, `2a8c663d`, `3d445fb6`, `18dca451`;
https://github.com/bazelbuild/rules_rust/pull/4181, https://github.com/bazelbuild/rules_rust/issues/1303,
https://github.com/bazelbuild/rules_rust/issues/3732, https://github.com/hermeticbuild/rules_rs/pull/255,
https://github.com/bazelbuild/rules_rust/discussions/2879, https://github.com/madesroches/micromegas/issues/1610,
https://bazelbuild.github.io/rules_rust/crate_universe_bzlmod.html
