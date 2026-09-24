# Bazel adoption decision

Repository: ocx-sh/ocx-mirror · Decided: 2026-09-23 · Decided by: owner (local loop, ratified
dossier `.agents/discussions/bazel-crate-split.md`); CI lane first by the pre-declared bar
`adr_bazel_crate_split.md` § C11 (NO-GO), then by `adr_bazel_full_adoption.md` § C1 (GO, 2026-09-23)

### Signals

| Signal | Measured value | Command | Answer |
|---|---|---|---|
| Cheaper fix tried and named | `Swatinem/rust-cache` on `Smoke (Linux)` since before this programme; warm nextest lane median 720 s | `grep -n rust-cache .github/workflows/verify.yml`; C11 run 35846656688 | yes |
| Median whole-repo CI wall-clock | 934 s over the last 10 green `Verify` runs on `main` | `gh run list --workflow verify.yml --branch main --status success --limit 10 --json createdAt,updatedAt` | 934 s |
| Largest CI job is a build job | `Smoke (Linux)` (build + unit test + ocx submodule build), 602–1312 s across the C11 legs | `measurement_bazel_ci_lane.md` | yes |
| Generator maturity per language | Rust: hand-written BUILD files (9), rules_rust 0.74.0 + crate_universe from the root `Cargo.lock`; `bazel:build:drift` keeps Cargo and BUILD edges equal; Python acceptance suite wrapped as one `sh_test` | `task bazel:build:drift`; `task bazel:test:unit` (14 targets, 1570 cases) | usable |
| Cross-repo coupling to model | `external/ocx` submodule: 9 ocx crates + 3 nested forks consumed as crate_universe path crates; rules_rust rewrites their BUILD files on repin, cured by the bootstrap restore (ADR A-4, A-9) | `task bazel:bootstrap` post-condition `git -C external/ocx status --porcelain` empty | yes, handled |
| Build owner after adoption | the owner; everything runs through `task`, nobody types `bazel` | (owner statement, dossier) | owner |

### Verdict

**go** for the local/agent loop (owner ruling) and **go** for CI, by `adr_bazel_full_adoption.md`,
which supersedes C11. C11's NO-GO (warm Bazel median 738 s vs warm nextest 720 s) was measured with no
remote cache; both rows its "What would change it" named are now true — the mirror reads and writes
`bazel-cache.ocx.sh` (the "cheaper fix" row), and CI keeps cargo only for fmt, clippy, the jsonschema
check and the release builds, as ocx does. The new bar is cacheability, not speed: two consecutive runs
of one tree, the second executing zero actions.

### What would change it
- The shared cache going away or losing its write lane — the lane then runs cold every time, and
  C11's speed comparison applies again.

### Explicitly not decided here
- Target selection in CI: `bazel:test:scoped` exists locally only.
- Remote execution: not applicable (`rules_ocx` launchers exec absolute host paths, as in ocx).
