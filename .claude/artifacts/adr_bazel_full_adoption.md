# ADR: Bazel for every CI lane, on the shared remote cache

## Metadata

**Status:** Accepted
**Date:** 2026-09-23
**Deciders:** Michael Herwig (owner — goal `.agents/goal/bazel-full-adoption.md`, stated verbatim there)
**GitHub Issue:** N/A (goal run)
**Related Design Spec:** N/A — ocx's `adr_bazel_build_adoption.md` is the design this copies
**Stack Alignment:**
- [x] Bazel 9.2.0 + rules_rust 0.74.0 as before (patched for the Windows host — C5); rules_python 2.3.4 (docs site, release provenance — C4, C5); the release legs add `hermetic_cc_toolchain` 4.3.0 (musl) and `apple_support` 1.24.2 (darwin) — C5
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

**C5 — Only release and cross-platform builds stay on cargo** (owner ruling 2026-09-24, overruling the
first cut of this clause, which kept fmt, clippy and the jsonschema check on cargo because ocx does). A
cargo step in `verify.yml` re-compiles or restores rust-cache on every run, so the job could never be
fully cached (C1). What stays: release and cross-platform builds (`build-matrix.yml`; ocx `release.yml`
has no Bazel, and ocx's darwin/windows legs keep nextest, ADR § Stage 2 ruling 1), and the macOS/Windows
arms of `task rust:lint` / `rust:verify` / `verify`. What moved:
- **Clippy:** `rust_clippy_aspect` under `.bazelrc` `build:clippy` (`task bazel:lint:clippy`), over
  `//...` — every first-party library, binary and test, no external target. Cargo's
  `warnings = "deny"` + `-D warnings` is `-Dwarnings`, and cargo's `--check-cfg` is passed too
  (`unexpected_cfgs`). A named config, not `rust_lint_config`: `lint_config` also adds its flags to
  the normal Rustc actions, re-keying every compile; aspect-only flags leave them unchanged (aquery:
  identical Rustc action key and output path with and without `--config=clippy`).
- **rustfmt:** `rustfmt_aspect` under `build:rustfmt` with `rustfmt.toml` as the label flag
  (`task bazel:lint:fmt`); `//:build_script_fmt` (manual, never compiled) carries build.rs, the one
  tracked `.rs` no target compiles. The fixer is `task rust:format:apply` — rustfmt over the tracked
  first-party files, no longer `cargo fmt --all`, which also rewrites every path dependency under
  external/ocx.
- **jsonschema:** `*_jsonschema` twins of the root, source, spec, error and pipeline libraries (the
  last two for Cargo's feature unification) plus a `manual` spec test twin, gathered by the
  `//:jsonschema` filegroup (`task bazel:build:jsonschema`). The optional `schemars` dependency is
  imported by its spoke name (`crates__schemars-<v>` in MODULE.bazel); `bazel:build:drift` reds a
  member feature without its variant rule.
- **Fork binding:** `bazel:patch:check` also reads Cargo.toml `[patch.crates-io]` and Cargo.lock
  (one path entry per fork, no `source`). The `cargo tree -i` step was no lockfile read: through the
  runner's rustup it installed the toolchain and downloaded every crate. `bazel:build:drift` reads
  `cargo metadata` through the Bazel toolchain's cargo (`@rules_rust//tools/upstream_wrapper:cargo`).

**C5 amended — the Linux musl release legs moved to Bazel** (phase L; darwin and windows still cargo).
`task bazel:build:release TARGET=<triple>` builds `//:ocx-mirror` for `//platforms:<triple>`:
- **Toolchain:** `hermetic_cc_toolchain` 4.3.0 (zig 0.15.2 — the linker family cargo-zigbuild used; no
  `rules_cc` bump, where `toolchains_musl` 0.1.27.bcr.1 needs 0.2.18) registers only its two
  `libc_aware` musl C toolchains, and a `rust.repository_set` adds rustc 1.95.0 for both musl triples
  with `-Clink-self-contained=no` (zig's crt and rustc's are otherwise a duplicate `_start`). Both are
  selected by `@zig_sdk//libc:musl`, which only `//platforms` carries: rules_rust gives a musl triple
  the gnu constraint set, so without it the gnu toolchains match a musl platform. crate_universe needs
  no musl triple (none exists in rules_rust 0.74.0's platform list): `cargo tree -e normal,build` is
  byte-identical for gnu and musl on both arches, so its gnu arms are exact.
- **Provenance:** `--define=ocx_mirror_provenance=release` selects `//release:provenance` over
  `testing_provenance.env` in `//:ocx_mirror`'s `rustc_env_files`. That rule reads the stable workspace
  status (`release/workspace_status.py`: build.rs's git fields, `VERGEN_BUILD_TIMESTAMP` under `CI`,
  `__OCX_BUILD_*`, `GITHUB_*`) and the target toolchain's triple and rustc, in one `no-remote-cache`
  action. Not rules_rust's `stamp` + `{KEY}` substitution: an absent key stays in the value verbatim,
  where build.rs omits the field. No `--stamp` (it would stamp every `rust_binary`).
- **Host keys unchanged:** execution-log keys of every host lane, before and after, 1143 of 1144
  identical; the one difference is a script test whose declared input `bazel.taskfile.yml` changed.
- **Parity** with the cargo-zigbuild artifact, both triples: static (no interpreter, no dynamic
  section), stripped, sizes within 0.02 %, `--json version` identical except the wall-clock build
  timestamp and the `commit` block, which only Bazel carried: the local cargo build's vergen-gix
  dropped it (`Could not determine status for submodule at 'external/ocx'` in a linked worktree; no
  CI log shows it). `ocx package create` accepts both. build-matrix.yml smokes every artifact
  (`task release:smoke`); `build-check.yml` runs the matrix on pull requests with channel `check`.

**C5 amended again — darwin and windows moved too; no release leg builds with cargo** (phases M and W,
owner: everything under Bazel). Same task, same `//release:provenance`, one native runner per OS family:
- **darwin:** `macos-latest` (arm64) builds both triples through `ocx exec bazel`; `apple_support` 1.24.2
  registers the Apple clang toolchains (`//platforms` carries its `apple`/`device` constraints). The
  task passes rustc's deployment targets as `--macos_minimum_os` (10.12 x86_64, 11.0 arm64 — unset, the
  toolchain targets the runner's SDK) and `--features=-link_libc++` (the binary holds no C++; the
  toolchain's `-lc++` added a load command cargo never had). x86_64 is smoked under Rosetta.
- **windows:** `windows-latest` (x64) builds both triples with rules_cc's MSVC toolchains (`x64`,
  `x64_arm64`) from the runner's Visual Studio — msvc ABI and dynamic CRT, as cargo-xwin built them. A
  `rust.repository_set` adds rustc for aarch64 on the x64 exec; `smoke-windows-arm64` smokes the arm64
  binary on `windows-11-arm`. Output root `C:/b` (MAX_PATH), `core.autocrlf false` (the templates ship
  LF), `MSYS_NO_PATHCONV` (Git Bash rewrote `//platforms:…`). What the Windows host needed besides:
  - `single_version_override` of rules_rust 0.74.0 with two patches under `release/`: crate_universe
    for path dependencies on a Windows host (`/D:/…` package ids, `\` in a rendered Starlark string, a
    bare `find` that is System32's find.exe), and one `-Ldependency` directory per Rustc action in the
    process wrapper. rustc puts every search path on PATH before loading proc macros; ocx_config's 350
    paths are 32488 characters, and past 32767 `LoadLibraryExW` fails with os error 8, reported as
    E0463 ([rust-lang/rust#110889](https://github.com/rust-lang/rust/issues/110889),
    [bazelbuild/rules_rust#3767](https://github.com/bazelbuild/rules_rust/issues/3767)). The wrapper's
    BUILD selects a patched copy of `main.rs` on Windows only: any byte of `main.rs` re-keys every
    Linux Rustc action (measured: 511 of 1080 test-lane keys moved).
  - The Windows repin runs the patched cargo-bazel, built from source; its `CARGO_BAZEL_GENERATOR_*`
    variables are set on that one command, or the build re-evaluates the crate extension, renders over
    external/ocx and stamps the binary dirty.
  - Crate annotations, per triple (`annotation_select`, so no host key moves): aws-lc-sys compiles with
    Visual Studio's clang-cl on aarch64 (MSVC `cl` ignores `.S`, D9027); octocrab's build script gets
    `CARGO_HOME` under `bazel-out` (its `cargo metadata` otherwise climbs to the execroot's
    Cargo.toml).
  - `BAZEL_WIN32_WINNT` emptied: rules_cc compiles C with `/D_WIN32_WINNT=0x0601`, and aws-lc took its
    Windows 7 path — an extra `bcrypt.dll!BCryptGenRandom` import cargo's SDK default never had.
- **Provenance action portable:** Python on rules_python's exec interpreter
  (`release/provenance_env.py`) instead of `sed`; the Linux env file and binary are byte-identical.
- **crate_universe:** both darwin and both windows triples in `supported_platform_triples`.
- **Host keys unchanged:** execution-log keys of the test, clippy and rustfmt lanes before and after:
  test 1079 of 1080 identical (the one: `bep_to_otlp_self_test`, whose declared input
  `bazel.taskfile.yml` changed), clippy 1056 of 1056, rustfmt 31 of 31.
- **Parity** with the cargo artifacts (build-check run 35942522657), in `.tmp/bazel-full/release-parity.md`:
  darwin load commands byte-identical (8 images), same minos/SDK; windows DLL lists identical (18
  x86_64, 17 aarch64); sizes within 1.4 %; `--json version` the same key set, equal where the value is
  not per run; `ocx package create` accepts every artifact. Bazel runs: darwin
  [35947180548](https://github.com/ocx-sh/ocx-mirror/actions/runs/35947180548), windows
  [35980500812](https://github.com/ocx-sh/ocx-mirror/actions/runs/35980500812).

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
| fmt, clippy, jsonschema check as Bazel lanes (aspects, `*_jsonschema` variants) | cargo steps in `verify-basic.yml` ("Check formatting", "Clippy"), `schema:generate` on cargo | C1: a cargo step re-compiles every run (C5) |
| Clippy/rustfmt scope = first-party targets | `cargo fmt --all` also checks the path dependencies; ocx's own crates are its members | the vendored ocx crates are ocx's to lint and format |
| `cargo metadata` for the drift gate via the Bazel toolchain | ocx's `bazel_build_drift.py` calls the host cargo | no host Rust toolchain in the CI job |
| Release builds (musl, darwin, windows) through `task bazel:build:release` | `release.yml` builds with cargo, no Bazel | owner: everything under Bazel (C5, amended twice) |
| rules_rust 0.74.0 under `single_version_override` with two `release/` patches | rules_rust unpatched | Windows host: crate_universe path dependencies; the process wrapper's `-Ldependency` PATH overflow (upstream issues named in C5) |
| `apple_support` 1.24.2 | no Apple toolchain module | the darwin legs' Apple clang toolchains |
| Crate annotations for aws-lc-sys (aarch64 windows) and octocrab (windows) | none | clang-cl for aws-lc's assembly; `cargo metadata` in octocrab's build script |

**No release exception is left.** An earlier revision kept release and cross-platform builds on cargo
(`build-matrix.yml`, `release.yml`) as ocx does. Phases L, M and W moved all six targets (C5 amendments):
musl on Linux with zig, darwin on a macOS runner, windows-msvc on a Windows runner. What still runs
cargo: the macOS/Windows arms of `task rust:lint` / `rust:verify` / `verify` (a developer on those hosts),
and `release.yml`'s `cargo metadata` version read — a manifest read, no compile.

## Consequences

- `verify.yml` runs no cargo and sets up no Rust toolchain; `Smoke (Linux)` replaces `acceptance-tests`
  and `bazel-graph`. No workflow compiles with cargo; every release binary is a Bazel build (C5).
- A cache outage degrades to a cold build (timeout 30 s, 2 retries), never a red.
- Residual acceptance under-declaration, stated plainly: running containers' state and mutable image tags
  (`registry:2`, tag-pinned images in ocx's compose file) are not in the key. Bounded by the host-wide flock
  and per-test repositories; `--nocache_test_results` clears a suspect entry for one run.
- `@tools` launchers bake the host's absolute `$OCX_HOME`; CI runners share one path, so CI→CI reuse holds
  and a developer host never reads CI's acceptance entry (over-keyed, not unsound).

## Evidence

**Pre-merge (branch runs read, never write — C3).** A cache *hit* cannot show before a `main` push has
written, so the pre-merge proof is that two runs ask the cache for identical keys. It uses the
`bazel-execlog` artifact (`scripts/bazel_execlog_keys.py`: one `<label> <mnemonic> <action digest>`
line per spawn). Left out: the `generate-xml.sh` spawn Bazel adds after a test *executes*, because it
reads that run's `test.log`. A cached test never runs it.

| Run | Tree | Unit | Acceptance | Site |
|---|---|---|---|---|
| [35923034539](https://github.com/ocx-sh/ocx-mirror/actions/runs/35923034539) | 813e6ca | 1590 processes, 1139 executed, 14/14 tests run | 1 local | 2 sandboxed |
| [35925312539](https://github.com/ocx-sh/ocx-mirror/actions/runs/35925312539) attempt 2 | 6a462b2 | same | same | same |
| [35925312539](https://github.com/ocx-sh/ocx-mirror/actions/runs/35925312539) attempt 3 | 6a462b2 | same | same | same |

Key files, attempt 2 vs attempt 3 (same tree): unit 1125/1125, acceptance 1/1 and site 2/2 lines,
all byte-identical. Run 1 vs run 2 (commits differing only outside these targets' inputs): also
byte-identical. Every action a second run would look up has the key the first run would have written.

After the lint lanes moved in (C5): runs [35934619032](https://github.com/ocx-sh/ocx-mirror/actions/runs/35934619032)
(d4f3085) and [35936219566](https://github.com/ocx-sh/ocx-mirror/actions/runs/35936219566) (2554774). Their key files are
identical for clippy (1126 lines), fmt (29), jsonschema (2), unit (29), acceptance (1) and site (2). Scripts differ in
exactly 2 of 18 lines: `bazel_build_drift_self_test` and `bep_to_otlp_self_test`, whose declared inputs
(`bazel_build_drift.py`, `verify.yml`, `bazel.taskfile.yml`) that commit edited.

**Local (done-bar 4).** After `bazel clean`, `bazel test //test:acceptance //docs:site` reported
`1523 processes: 1099 disk cache hit, 424 internal` and `Executed 0 out of 1 test`.

**Post-merge (the C1 bar itself).** Pending the owner's merge. The first `main` push run writes the
cache. A re-run of it (`gh run rerun <id>`) must then report `Executed 0 out of 14` (unit) and
`0 out of 1` (acceptance), and zero non-internal processes in all three Bazel steps.
