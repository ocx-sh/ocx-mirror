---
paths:
  - cliff.toml
  - CHANGELOG.md
  - .github/workflows/release.yml
  - .github/workflows/verify-version.yml
  - .github/workflows/backfill.yml
  - .github/workflows/canary.yml
  - .github/workflows/prune.yml
  - .github/actions/release-ocx/**
  - taskfiles/release.taskfile.yml
---

# Release Implementation Notes

Guidance for release + install strategy (signing ADR: `adr_mirror_signing.md`). Read when working on release infra, versioning, or changelog.

## Key Decisions

### Release = a dispatched, hand-written workflow

`.github/workflows/release.yml` is hand-written and runs only on `workflow_dispatch`. A release or candidate runs from `refs/heads/main` of the ocx-sh/ocx-mirror repository (`plan` checks the repository id). Edit it like any other workflow; no generator owns it.

Inputs: `mode`, `version`, `prune_after_days` (default 30, the age of a candidate of a shipped version; `prune.yml` has its own input of that name).

| Mode | `version` | Builds and publishes |
|---|---|---|
| `release` | `X.Y.Z`, equal to the `Cargo.toml` version; refused when tag `vX.Y.Z` exists | `ghcr.io/ocx-sh/ocx/mirror` (logical `ocx.sh/ocx/mirror`) as `X.Y.Z_<TS>` plus the cascade tags, then tag `vX.Y.Z`, GitHub Release, announce, prune of old candidates |
| `candidate` | `X.Y.Z-rcN`, same `X.Y.Z` as `Cargo.toml` | the same package as `X.Y.Z-rcN_<TS>` plus its own rolling tag; GitHub pre-release, ephemeral announce |
| `dev` | ignored; `X.Y.Z` is the `Cargo.toml` version | no git tag, no GitHub Release. The track follows the dispatched ref. `X.Y.Z-canary` from `refs/heads/main` goes to the release package and is announced as an ephemeral index row. `X.Y.Z-pr<N>` from a branch with an open pull request and `X.Y.Z-dev` from any other branch go to `ghcr.io/ocx-sh/dev/mirror`, unannounced; a pull request build also gets a sticky pull request comment with its address |

Jobs, in order: `verify-version` (`verify-version.yml`: version against `Cargo.toml`), `plan` (guards, track, version, tag, target package), `verify` (`basic-repo.yml`) and `readiness` (`readiness-gate.yml` on `github.sha`) for release and candidate, `build` (`build-matrix.yml`) beside `sbom`, `publish`, `github-release`, `announce`, `prune` (release mode only: candidates of a shipped version); a pull request build adds `comment`. Dev, pull request and canary builds are pruned by `prune.yml`, never by a tail job of this file. A release and a candidate share one concurrency group; a dev run queues per ref, so an automatic canary cannot evict a waiting release.

- **Release builds.** `build-matrix.yml` cross-builds six triples on one Linux runner with `task bazel:build:release TARGET=<triple>`: real provenance and no cache credential (`release.yml` maps no cache secret to `build`). Linux musl legs smoke in the build job with `task release:smoke`; darwin and windows binaries smoke on native runners (`smoke-native`) before `publish` sees them. The `sbom` job writes one CycloneDX document per triple with `scripts/release_sbom.py`.
- **Sign before tag.** `publish` pushes every platform with `--sign` and an SBOM (`scripts/publish_mirror_package.sh`), verifies the signature and the CycloneDX attestation of each platform, pushes the catalog description, and only then applies the cascade once with `ocx package cascade repair`. It verifies against `RELEASE_IDENTITY` (the literal `…/release.yml@refs/heads/main`) for anything that lands in the release package (a release, a candidate, a canary), and against its own `${GITHUB_SERVER_URL}/${GITHUB_WORKFLOW_REF}` for the dev package. `github-release` needs `publish`, so no tag exists before a signature does. Exactly two workflows sign: `release.yml` (job `publish`) and `backfill.yml`.
- **Windows names.** Release assets are `ocx-mirror-<triple>.tar.xz` (unix) and `.zip` (windows), plus `sha256.sum`; the windows triples are `*-pc-windows-gnullvm`.
- **`backfill.yml`** (dispatch, `refs/heads/main`, environment `release-backfill`, 120 minute timeout) signs and attests the platforms of already published versions that lack either, and never moves a tag. Inputs: `tags` (empty = every version tag), `dry_run` (default true), `limit` (stop after that many versions needing work; dispatch again to continue). It trusts the `release.yml` identity as well (`--also-trusted`), so what `release.yml` signed is not signed again.
- **`canary.yml`** fires on `workflow_run` of a green `verify-basic` push run on main of this repository and dispatches `release.yml --ref main -f mode=dev`, only while that commit is still the main tip. It holds `actions: write` and checks out nothing.
- **Canary in the index.** A run on main (track `canary`) publishes to the release package and `announce` runs although `github-release` is skipped (its `if` passes a skipped `github-release` for that track alone). The rows are ephemeral (`--ephemeral` for everything but a release). PR and `-dev` builds stay in the dev package: they run from a branch, and only a run from main signs as the release identity. A canary and a release are signed under that same `RELEASE_IDENTITY`, so a verifier that pins it accepts canaries as well as releases.
- **`prune.yml`** (daily `23 4 * * *` plus `workflow_dispatch` with `prune_after_days`, 30 on a schedule; `refs/heads/main` and repository guard first in both jobs; concurrency group `prune`) owns every prune except release mode's candidates. Selection is `scripts/release_prune_select.py`. Job `dev` prunes the dev package: `-dev` builds by age except the newest per version, `-pr<N>` builds once pull request N is no longer open or the build is past `prune_after_days`; a family's rolling tag goes with its last build. It is the only job that passes `--force`. Job `canary` (environment `ocx.sh`) keeps the newest `CANARY_KEEP` (3) canary builds, announces the selection with `--ephemeral --tags-file`, prunes it, then runs `announce --refresh`. The announce comes first because `prune` (no `--force`) refuses the whole run with exit 75 while one selected tag is unlisted.
- **Pull request comment** (`comment`, track `pr<N>` only): one comment per pull request, found by the marker `<!-- ocx-pr-build -->` and the author `github-actions[bot]`, edited in place; it carries the rolling and the exact OCI address. The job holds `pull-requests: write` and nothing else.
- **Structural tests.** `scripts/test_release_workflows.py` (a `py_test` in `scripts/BUILD.bazel`, run by `task scripts:self-test`) pins who may sign, dev isolation from the release package and index, `--force` on the dev package only, the pre-release and latest flags, the tag and release coming from `github-release` after `publish`, the readiness gate, the pinned identity, the floating-tag order, the prune reconciliation, the cache-free stamped build and the `release-ocx` pin verification. Each family has a negative control that mutates the parsed YAML and must turn red.

### The release ocx

Every job that publishes, announces, prunes or backfills installs its `ocx` through `.github/actions/release-ocx`, never from the runner's own. The pin is `[group.release.tools] ocx` of the root `ocx.toml` (with `git-cliff` and `cargo-cyclonedx`, reached with `ocx exec -g release --`), locked in `ocx.lock` (lock version 4). The action bootstraps with `setup-ocx`, verifies the locked digest against the identity in `.github/actions/release-ocx/ocx-identity.env`, then puts the pinned binary first on `PATH` and asserts it is not the bootstrap. `scripts/release_ocx_identity.py` allows an ocx signed from a branch only in mode `dev`; `release`, `candidate`, `canary`, `prune` and `backfill` need the identity of ocx's main release workflow. A new pin swaps `ocx-identity.env` in the same change.

### Testing an unreleased build

For cross-repository and e2e work against a build that is not released (runbook: skill `e2e-test`, tier 2):

- **Main.** `ocx.sh/ocx/mirror:<X.Y.Z>-canary` follows the newest canary; the newest three canaries stay. Never dispatch `mode=dev` on main to test a branch: it publishes a canary into the release package.
- **A pull request.** Dispatch `gh workflow run release.yml --ref <branch> -f mode=dev`, copy the address from the pull request comment, and pin it in the other repository. The rolling `<X.Y.Z>-pr<N>` tag follows the newest build of that pull request; the exact `…_<timestamp>` tag pins one build.
- **Signer.** A pull request build is signed by `…/release.yml@refs/heads/<branch>`, not the release identity.
- **Lifetime.** A pull request build is removed after the pull request closes (the next `prune.yml` run sweeps it) or after `prune_after_days`.

### Version Source of Truth

`[workspace.package] version` in root `Cargo.toml` = single source of truth. The root package and the member crates inherit it via `version.workspace = true`. `release:bump-files` also moves the `MODULE.bazel` `module(version=)` and the `version` attributes of the root and `crates/*` `BUILD.bazel` files; `bazel:pin:check` and `bazel:build:drift` fail when they differ.

### Commit Convention

All commits must follow [Conventional Commits](https://www.conventionalcommits.org/). cocogitto checks pull requests in `basic-repo.yml`; git-cliff generates the changelog from the subjects. `cliff.toml` is template-owned and renders a commit's subject only, with `<` and `>` escaped.

## Implementation Constraints

### Package Directory Structure

The package pushed to `ocx.sh/ocx/mirror` holds `bin/ocx-mirror` (`bin/ocx-mirror.exe` for a windows platform) and nothing else; metadata is `packaging/metadata.json`. A different layout breaks consumers that resolve the binary from `bin/`.

### Dependency Update Config

Dependency updates run via **Renovate**. `renovate.json` is template-owned: it extends the `ocx-sh/conventions` preset and `local>ocx-sh/ocx-mirror//.github/renovate-local`. Repo-owned rules live in `.github/renovate-local.json`.

## Workflow: Release Ceremony

Release ceremony = human-driven process with tooling support, in four steps. No step tags or pushes for you; the release is a workflow dispatch:

```bash
# 1. Prepare. Never tags or pushes.
task release:prepare BUMP=auto   # or BUMP=patch|minor|major, or VERSION=x.y.z
# Human reviews the changes
# 2. Commit, land the release commit on main, dispatch readiness on that exact commit.
git commit -am "release: vX.Y.Z"
gh workflow run release-readiness.yml --ref <ref holding the release commit>
# 3. Print the dispatch command, then run it yourself.
task release:tag
# gh workflow run release.yml -f mode=release -f version=X.Y.Z
# 4. release.yml takes over: verify, readiness gate, build, sign, publish, tag, GitHub Release, announce.
```

1. **`task release:prepare`** — the conventions template's task (`taskfiles/baseline.taskfile.yml`, template-owned); gate, bump, changelog, verify hook. It asks for no readiness run: the bump commit does not exist while it runs.
2. **Commit, land, dispatch.** `release-readiness.yml` builds all six triples (`build-matrix.yml`, channel `check`), then runs `acceptance.yml` on linux musl (Docker), macOS arm64 and Windows gnullvm against `test/limits/<os>-release`. It publishes nothing. The run-name is `readiness <sha>`. A dispatch with `bypass=true` and a `reason` skips every test job, goes green, and renames the run `readiness <sha> BYPASS: <reason>`; use it for an outage of the gate itself, never for a red result. The label `run-readiness` on a pull request runs it too.
3. **`task release:tag`** (`taskfiles/release.taskfile.yml`) — runs `release:guard:ocx-released`, reads the version from `Cargo.toml` and prints `gh workflow run release.yml -f mode=release -f version=<version>`. It never runs it, tags nothing and pushes nothing. `release.yml` refuses to run off `refs/heads/main`, outside this repository, or when tag `v<version>` already exists. Use `mode=candidate -f version=X.Y.Z-rcN` for a release candidate and `mode=dev` for a dev build.
4. **`readiness-gate.yml`** — the `readiness` job calls it on `github.sha` (`scripts/release_readiness_gate.py`); `publish` needs it, so a red gate publishes nothing. `gh workflow run readiness-gate.yml --ref <ref> -f sha=<sha>` shows the gate for any commit without cutting a release.

`release:prepare` calls hooks that `taskfiles/release.taskfile.yml` defines, in this order:

| Step | What it does |
|---|---|
| `release:gate` | `release:guard:ocx-released`: the committed `external/ocx` pointer is contained in an ocx tag published on origin. Defining it replaces the template's check of the latest verify-deep run. |
| `release:bump-files` | `release:guard:version-unreleased` (no tag `vX.Y.Z` on origin), `cargo set-version`, then the `MODULE.bazel` and `BUILD.bazel` version twins and `MODULE.bazel.lock`. |
| git-cliff | Regenerates `CHANGELOG.md` from the commit subjects. |
| `release:verify` | Echoes and nothing else: no local suite runs, readiness on the release commit is the test gate. It prints the next steps for the version in `Cargo.toml`. |

The gate runs before any file is touched, so a refused release leaves the tree as it was.

### Recovery: publish green, a later job red

Once `publish` is green, the floating tags, signatures and attestations exist. What can still be missing: the git tag, the GitHub release, the index row.

- **Rerun, do not redispatch.** Use "Re-run failed jobs" on the same run. `plan` outputs and the build tag are reused, so no new build tag, signature or attestation referrer appears.
- **Artifacts expire.** `published-tags` is kept 7 days; the binaries (`ocx-mirror-<triple>`) and `release-sbom` are kept 1 day. Later, `announce` and `github-release` cannot fetch their input.
- **`github-release`** creates the tag at the run's SHA, and `gh release create` fails when the release exists. Run `gh release view vX.Y.Z` before rerunning.
- **`announce` and `prune`** run in the `ocx.sh` environment. `announce` refuses an empty `published-tags.txt` or a missing token. `prune` runs in release mode only; dev, pull request and canary builds are `prune.yml`'s.
- **Fresh dispatch is the last resort** (artifacts expired, or the run cannot be rerun). `plan` refuses a non-main ref, the wrong repository, a malformed version and an existing `refs/tags/vX.Y.Z`; it does not refuse a published-but-untagged version. A fresh dispatch builds a new `X.Y.Z_<timestamp>` build tag, adds one more signature and attestation referrer per platform, and re-cascades the floating tags.

Afterwards check: git tag, GitHub release and index row.

### Version Between Releases

Between releases, `Cargo.toml` retains the version from the last `release:prepare` run, and dev builds carry it. The task computes the next version from commit history via `git-cliff --bumped-version` at release time (or takes `VERSION=`). No automated post-release version bump — `release:prepare` = single versioning mechanism.

## Cross-References

- **Registries and channels**: `AGENTS.md` § "Registries"; the tier-2 runbook is `.claude/skills/e2e-test/references/dev-channel.md`
- **Commit conventions**: `.claude/rules/workflow-git.md`
- **Mirror module map and error model**: `.claude/rules/subsystem-mirror.md`
