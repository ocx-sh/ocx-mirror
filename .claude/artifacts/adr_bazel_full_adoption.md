# ADR: Bazel for every CI lane, on the shared remote cache

## Metadata

**Status:** Accepted
**Date:** 2026-09-23
**Deciders:** Michael Herwig (owner — goal `.agents/goal/bazel-full-adoption.md`, stated verbatim there)
**GitHub Issue:** N/A (goal run)
**Related Design Spec:** N/A — ocx's `adr_bazel_build_adoption.md` is the design this copies
**Stack Alignment:**
- [x] Bazel 9.2.0 + rules_rust 0.74.0 as before; one new ruleset, rules_python 2.3.4 (docs site only — see C4)
**Domain Tags:** ci | docs
**Supersedes:** `adr_bazel_crate_split.md` § C11 (the CI lane-swap bar) and the NO-GO it produced in `measurement_bazel_ci_lane.md`
**Superseded By:** N/A

## Context

C11 measured the Bazel CI lane against nextest **without a remote cache** (`--disk_cache` restored through
`actions/cache`) and returned NO-GO: warm Bazel median 738 s vs warm nextest 720 s. C11 itself names the
way out — "a new ADR stating a new bar, measured on fresh runs" — and `decision_bazel_adoption.md` names
the missing input: "a remote cache for the mirror (ocx reads `bazel-cache.ocx.sh`)". Since then the org
secrets `BAZEL_CACHE_READ_AUTH` / `BAZEL_CACHE_WRITE_AUTH` were made visible to this repository, and ocx
runs `bazel test` in its `Smoke` job against that cache (ocx `verify-basic.yml`, PR
[ocx-sh/ocx#499](https://github.com/ocx-sh/ocx/pull/499)).

The owner's goal replaces the bar: *all* parts — compilation, unit and acceptance tests, the website — run
through Bazel, and **two consecutive CI runs on one tree are fully cached**. The `__testing` feature and
`testing_provenance.env` exist so the test binary's key does not move per commit.

## Decision

**C1 — The new bar (replaces C11).** A Bazel CI lane is GO when, for two consecutive CI runs of the same
tree after the write lane has run once, the second run executes **zero** actions and reports every test
`(cached)` in every Bazel step (unit, acceptance, docs site); plus `task verify` green and the per-case
JUnit count equal to the libtest total (C11 (3), kept). Wall-clock versus nextest is no longer the bar:
a fully cached run is bounded by setup, not compilation, and the owner ruled on hermetic reuse, not speed.
Evidence goes in § Evidence below.

**C2 — Cache wiring = ocx's, verbatim.** `.bazelrc` carries ocx's block: `--remote_cache=https://bazel-cache.ocx.sh/v1`,
`--remote_upload_local_results=false`, `--remote_timeout=30s`, `--remote_retries=2`. The credential rc is
written per job by `.github/actions/bazel-cache-rc` (copied from ocx; read = `--remote_header`, write =
`--credential_helper`, never both). Developers read through a header in `~/.bazelrc`. The **`/v1`
generation is shared with ocx**: same host, same realm, same write secret — so no trust boundary separates
the two repos' entries anyway, and third-party crate compiles hit across repos. A poisoned generation is
abandoned for both at once by moving both rc files to `/v2`.

**C3 — One write lane.** `verify.yml` job `Smoke (Linux)`, on
`github.event_name == 'push' && github.ref == 'refs/heads/main' && github.workflow == 'Verify'`
(ocx ruling 5, plus the third term because `verify.yml` is also a `workflow_call` target of `release.yml`,
ocx ruling 5a). The grant `--remote_upload_local_results=true` is a CLI argument on that expression only.
Every other lane — pull requests, `docs.yml`, the tag run — reads; a fork PR runs on `--disk_cache` alone.
Consequence, as in ocx (plan DX-93): a pull request can only hit what a `main` push wrote.

**C4 — Everything CI runs under Bazel is remote-cacheable.**
- Unit tests: `bazel test //crates/... //:all` — unchanged targets; all four former `[[excluded]]` cases
  now run under Bazel (`crates/TEST_TARGET_MAP.toml` has no exclusion list), so CI needs no nextest.
- Acceptance: `//test:acceptance` result is **uploaded** on the write lane. Sound because every input it
  reads is declared: the Sigstore stack comes from `@ocx_test` (a `new_local_repository` over the pinned
  submodule), `test/acceptance.stamp` (which carried a HEAD) is gone, `env_inherit` is down to
  `DOCKER_*`/`HOME`/`USER`/`XDG_RUNTIME_DIR`, the suite runs with a fresh `HOME` and a pinned uv Python.
  `bazel:tag:guard` reds any cache-suppressing tag and any acceptance closure that loses the binary, the
  compose file or the pinned ocx.
- Docs site: `//docs:site` — mkdocs `--strict` over a hashed `docs/requirements.lock` through rules_python
  `pip.parse`, sandboxed, offline, byte-deterministic (`SOURCE_DATE_EPOCH=0`, sorted tar). `docs.yml`
  builds the same target read-only and deploys the archive.

**C5 — What stays on cargo, each because ocx keeps the same thing on cargo.** `cargo fmt --check` and
`cargo clippy` (ocx `verify-basic.yml` "Check formatting"/"Clippy"), the jsonschema feature check (ocx
keeps `schema:generate` on cargo), release and cross-platform builds (`build-matrix.yml`; ocx
`release.yml` has no Bazel, and ocx's darwin/windows legs keep nextest, ADR § Stage 2 ruling 1). The
`cargo tree -i` fork-binding assertion stays too: a lockfile read, guarding the cargo-built release
binaries' `[patch.crates-io]` table, which only this repository re-declares.

## Deviations from ocx

| Mirror | ocx | Why |
|---|---|---|
| Acceptance results uploaded to the remote cache | acceptance lane read-only, results never leave the host (`test/bazel.bzl`) | C1 bar; soundness bought by full input declaration (C4) |
| Site remote-cacheable, built in CI and deployed from Bazel | `site.bzl` tags `no-remote-cache`/`requires-network`; `deploy-website.yml` runs vitepress directly | C1 bar; pip deps resolved by repository rules, so the action is offline |
| `--config=ci` wired | `build:ci` block latent | a cached run downloads only test reports |
| Acceptance in the `Smoke` job | separate `verify-deep.yml` job | one output base, one write lane |
| `github.workflow == 'Verify'` in the write gate | not needed (verify-basic is not reusable) | ocx ruling 5a |
| Execution-log key artifact per run | none | pre-merge evidence that two runs ask for identical keys (C1 needs a `main` push for hits) |
| rules_python for the docs interpreter | ocx fetches no interpreter via http_archive | `@tools` exposes executables only, no site-packages |

## Consequences

- CI no longer builds with cargo except the C5 steps; `Smoke (Linux)` replaces `acceptance-tests` and
  `bazel-graph`.
- A cache outage degrades to a cold build (timeout 30 s, 2 retries), never a red.
- Residual acceptance under-declaration, stated plainly: running containers' state and mutable image tags
  (`registry:2`, tag-pinned images in ocx's compose file) are not in the key. Bounded by the host-wide flock
  and per-test repositories; `--nocache_test_results` clears a suspect entry for one run.
- `@tools` launchers bake the host's absolute `$OCX_HOME`; CI runners share one path, so CI→CI reuse holds
  and a developer host never reads CI's acceptance entry (over-keyed, not unsound).

## Evidence

_Filled from CI — run URLs, executed-action counts, cache-hit lines._
