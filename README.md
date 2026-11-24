<div align="center">

<img src="./assets/logo.svg" width="192" />

# ocx-mirror

</div>

Mirror upstream tool releases (GitHub Releases, URL indexes) into any OCI
registry as [OCX](https://github.com/ocx-sh/ocx) packages. YAML-configured,
two-phase pipeline (concurrent prepare, sequential push with cascade tagging),
generated GitHub Actions CI pipelines with per-platform smoke tests.

- **Documentation**: <https://ocx-sh.github.io/ocx-mirror/>
- **Install**: `ocx --global add ocx.sh/ocx/mirror`

## Development

This repository vendors ocx as a git submodule (`external/ocx`) and consumes
ocx's `ocx_*` crates as path dependencies into it — clone recursively:

```sh
git clone --recurse-submodules https://github.com/ocx-sh/ocx-mirror.git
```

Toolchain bootstraps via [direnv](https://direnv.net) + `ocx direnv export`
(see `ocx.toml`). Common tasks:

```sh
task            # fast check (fmt, clippy, cargo check)
task verify     # full gate (lint, licenses, build, Bazel gates on Linux, unit + acceptance tests)
task test       # acceptance tests (needs Docker for the local registry)
```

The Linux Bazel loop, test telemetry and the rest of the task list:
[docs/contributing.md](./docs/contributing.md).

## Bumping ocx (the `external/ocx` submodule)

The `ocx_*` crates are not published — their version is whatever the submodule
points at. To advance:

```sh
OLD=$(git ls-tree HEAD external/ocx | awk '{print $3}')         # for the Bazel-pin diff below
git -C "$(git rev-parse --show-toplevel)/external/ocx" fetch origin && git -C "$(git rev-parse --show-toplevel)/external/ocx" checkout origin/main
git -C "$(git rev-parse --show-toplevel)/external/ocx" submodule update --init --recursive   # nested fork submodules
cargo check                                               # refreshes Cargo.lock
task verify
git add external/ocx Cargo.lock && git commit -m "chore(deps): bump external/ocx"
```

Checklist when bumping:

- keep `rust-toolchain.toml` channel in sync with `external/ocx/rust-toolchain.toml`,
  and `MODULE.bazel`'s `toolchains.toolchain(version = ...)` with that channel
  (`task bazel:pin:check` reds on a mismatch)
- keep the dependency feature lists in root `Cargo.toml`'s
  `[workspace.dependencies]` in sync with ocx's `[workspace.dependencies]`
- the `[patch.crates-io]` table must keep pointing at the nested fork
  submodules (`external/ocx/external/...`) — see the comment in `Cargo.toml`
  (`task bazel:patch:check` asserts the same binding in `Cargo.lock`)
- keep the Bazel pins copied from ocx in sync: `rules_shell` and
  `buildifier_prebuilt` `bazel_dep` versions, and
  `.bazelversion` / `ocx.toml`'s bazel tag against `external/ocx/MODULE.bazel`
  and `external/ocx/.bazelversion`
  (`git -C "$(git rev-parse --show-toplevel)/external/ocx" diff $OLD..HEAD -- MODULE.bazel .bazelversion ocx.toml`).
  `rules_ocx` is not copied: this repository pins the one whose bootstrap `ocx`
  reads the root `ocx.lock` (see the comment at its `git_override` in `MODULE.bazel`)

Every bump also moves the pinned `ocx` CLI to the newest ocx **release**, as
its own commit. The linked crates and the spawned CLI are separate pins: a
pointer bump alone leaves `task verify` and every generated workflow on the old
binary.

```sh
V=$(gh release view --repo ocx-sh/ocx --json tagName --jq .tagName); V=${V#v}
OLD=$(grep -oE 'ocx\.sh/ocx/cli:[0-9.]+"' ocx.toml | tr -d '"' | cut -d: -f2)   # the [tools] pin; the release one ends in _<timestamp>
names() { gh release view "v$1" --repo ocx-sh/ocx --json assets --jq '.assets[].name' | sort; }
comm -3 <(names "$OLD") <(names "$V")              # must print nothing, see below
sed -i -E "s|(ocx\.sh/ocx/cli:)[0-9]+\.[0-9]+\.[0-9]+\"|\1$V\"|" ocx.toml
sed -i -E "s|(OCX_CONTAINER_CLI_TAG: &str = \"v)[0-9]+\.[0-9]+\.[0-9]+|\1$V|" \
  src/command/package/pipeline/generate/ci/matrix.rs
sed -i -E "/uses: ocx-sh\/setup-ocx@/,+3 s|(version: ['\"])[0-9]+\.[0-9]+\.[0-9]+|\1$V|" \
  $(git grep -l 'ocx-sh/setup-ocx@' -- .github)
ocx lock                                           # re-resolves only the changed binding
cargo test -p ocx_mirror golden                    # must FAIL: the goldens assert the pin
UPDATE_GOLDEN=1 cargo test -p ocx_mirror golden    # regenerate, then rerun without the variable
```

- `ocx.toml` / `ocx.lock` — the child `task verify` and the acceptance suite run.
  The `[group.release.tools]` `ocx` is a different pin, the build that publishes;
  it moves with `.github/actions/release-ocx/ocx-identity.env`, not here.
  Bazel reads `ocx.lock` through the `ocx` that `rules_ocx` bootstraps (0.6.6),
  which reads lock versions 3 and 4. A `lock_version` line in the diff means the
  bootstrap must read the new version too: `task verify:scoped` shows it, `cargo`
  and the goldens do not
- `OCX_CONTAINER_CLI_TAG` in `src/command/package/pipeline/generate/ci/matrix.rs`
  — the `ocx` every generated workflow's test legs install; the `tests/golden/`
  diff must be that one `version:` line per `setup-ocx` step and nothing else
- every `ocx-sh/setup-ocx` `version:` step under `.github/`
- the `comm` of release archive names: `setup-ocx` downloads an archive by name, so
  a renamed one is a 404 on that OS. ocx 0.6.7 renamed the Windows archives from
  `-msvc` to `-gnullvm`, which `setup-ocx` reads from v1.4.1 on. When the `comm`
  prints anything, move the `setup-ocx` action pin first, in `.github/` and in
  `src/command/package/pipeline/generate/templates/`
- not reachable from here: `ocx-sh/conventions`' `setup` action carries its own
  `ocx` version and moves with a conventions release (`task copier:update`)

The floor prose in `.claude/rules/subsystem-mirror.md` and
`docs/reference/environment.md` names the *minimum* `ocx`, not the pin: it moves
only when the mirror starts spawning a subcommand or flag an older `ocx` rejects.

To build ahead of an ocx release, point the submodule at the unreleased commit
(a provisional pointer) and re-point it at the release tag before this
repository's own release — a mirror release must never ship before the ocx
release it pins.

## Bumping the pinned uv crates

`ocx_python` parses `pylock.toml`, wheel filenames, and PEP 508/440 markers via
four git-pinned crates from [astral-sh/uv](https://github.com/astral-sh/uv).
Their rows live in ocx's `[workspace.dependencies]` (`external/ocx/Cargo.toml`),
not here — the mirror reaches the uv types through `ocx_python::`. A uv bump is
therefore an ocx PR (`/ocx-upstream-pr`) followed by a submodule pointer bump.

## Mirror development against unreleased ocx changes

Point the submodule at an ocx feature branch, develop, and land the ocx side
first; then bump the submodule here to the landed commit.

## Releases

```sh
task release:prepare   # refuse an unreleased ocx pointer, compute version (git-cliff),
                       # bump Cargo.toml + the MODULE.bazel/BUILD version twins,
                       # refresh MODULE.bazel.lock, update CHANGELOG
                       # then print the steps below with the version filled in
# review, then commit and push the release commit:
git commit -am "release: vX.Y.Z"
git push origin main
# prove that exact commit: dispatch Release Readiness and wait for green
gh workflow run release-readiness.yml --ref main
task release:tag       # prints the dispatch command, never runs it
gh workflow run release.yml -f mode=release -f version=X.Y.Z
```

`task release:prepare` needs no readiness run: the commit it prepares does not exist yet.
Release Readiness builds all six triples and runs the acceptance suite on three of them.
`release.yml` demands a green run on the dispatched commit, and `publish` waits for the answer.
A release commit that lands through a pull request gets a new SHA on main; run readiness for that SHA.
A new workflow file cannot be dispatched until it is on the default branch; use the label until then.
A dispatch with `bypass` needs a `reason`; the run-name carries it and the gate prints a warning.

`release.yml` (mode `release`) signs and publishes `ocx.sh/ocx/mirror:<version>_<timestamp>` (+ cascade
tags), then creates the tag and a GitHub Release; mode `candidate` ships `X.Y.Z-rcN` as a pre-release.
Mode `dev` creates no tag and no GitHub Release, and its track follows the dispatched ref. `X.Y.Z` is the
`Cargo.toml` version.

| Ref | Track | Published as |
|-----|-------|--------------|
| `main` | `canary` | `ocx.sh/ocx/mirror:X.Y.Z-canary_<timestamp>` + rolling `X.Y.Z-canary`, announced as an ephemeral index row |
| branch with an open pull request | `pr<N>` | `ghcr.io/ocx-sh/dev/mirror:X.Y.Z-pr<N>_<timestamp>` + rolling `X.Y.Z-pr<N>`; one sticky pull request comment carries the address |
| any other branch | `dev` | `ghcr.io/ocx-sh/dev/mirror:X.Y.Z-dev_<timestamp>` + rolling `X.Y.Z-dev` |

`canary.yml` dispatches the canary after every green `verify-basic` run on `main`.
Do not dispatch `mode=dev` on `main` to test a branch: it publishes a canary into the release package.

`prune.yml` runs daily and by dispatch (`prune_after_days`, default 30).
It removes dev builds past that age except the newest per version, builds of closed pull requests, and builds of open ones past the age.
It keeps the 3 newest canary builds and refreshes the index.
`release.yml` prunes only the candidates of a released version, in release mode.

`backfill.yml` signs and attests versions published before `release.yml` did and never moves a tag.
Its inputs are `tags`, `dry_run` and `limit`: a dispatch stops after `limit` versions, and the next dispatch continues.
It skips what `release.yml` already signed.

## License

Apache-2.0 — see [LICENSE](./LICENSE).
