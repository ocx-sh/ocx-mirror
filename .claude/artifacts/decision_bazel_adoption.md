# Bazel adoption decision

Repository: ocx-sh/ocx-mirror · Decided: 2026-09-23 · Decided by: owner (local loop, ratified
dossier `.agents/discussions/bazel-crate-split.md`); CI lane by the pre-declared bar
`adr_bazel_crate_split.md` § C11

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

**go** for the local/agent loop (owner ruling), **no-go** for the CI test lane, because C11
criterion 1 failed: warm Bazel median 738 s vs warm nextest 720 s (bar ≤ 540 s and ≥ 120 s
saved).

### What would change it
- A new ADR stating a CI bar where the Bazel lane also keeps a cargo cache for the steps Bazel
  does not replace (ocx submodule build, clippy) — reopens the "Largest CI job" row.
- A remote cache for the mirror (ocx reads `bazel-cache.ocx.sh`) — reopens the cheaper-fix row.

### Explicitly not decided here
- Target selection in CI: `bazel:test:scoped` exists locally only.
- Remote execution: not applicable (no remote cache either).
