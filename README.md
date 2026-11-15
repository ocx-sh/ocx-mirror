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
- keep the Bazel pins copied from ocx in sync: `rules_shell`,
  `buildifier_prebuilt` and `rules_ocx` `bazel_dep` versions, and
  `.bazelversion` / `ocx.toml`'s bazel tag against `external/ocx/MODULE.bazel`
  and `external/ocx/.bazelversion`
  (`git -C "$(git rev-parse --show-toplevel)/external/ocx" diff $OLD..HEAD -- MODULE.bazel .bazelversion ocx.toml`)

Every bump also moves the pinned `ocx` CLI to the newest ocx **release**, as
its own commit. The linked crates and the spawned CLI are separate pins: a
pointer bump alone leaves `task verify` and every generated workflow on the old
binary.

```sh
V=$(gh release view --repo ocx-sh/ocx --json tagName --jq .tagName); V=${V#v}
sed -i -E "s|(ocx\.sh/ocx/cli:)[0-9]+\.[0-9]+\.[0-9]+|\1$V|" ocx.toml
sed -i -E "s|(OCX_CONTAINER_CLI_TAG: &str = \"v)[0-9]+\.[0-9]+\.[0-9]+|\1$V|" \
  src/command/package/pipeline/generate/ci/matrix.rs
sed -i -E "/uses: ocx-sh\/setup-ocx@/,+3 s|(version: ['\"])[0-9]+\.[0-9]+\.[0-9]+|\1$V|" \
  $(git grep -l 'ocx-sh/setup-ocx@' -- .github)
G=$(sed -nE 's/^generated_by = "ocx (.*)"/\1/p' ocx.lock)   # the ocx that wrote the lock
ocx package install "ocx.sh/ocx/cli:$G"
ocx package exec "ocx.sh/ocx/cli:$G" -- ocx lock   # re-resolves only the changed binding
cargo test -p ocx_mirror golden                    # must FAIL: the goldens assert the pin
UPDATE_GOLDEN=1 cargo test -p ocx_mirror golden    # regenerate, then rerun without the variable
```

- `ocx.toml` / `ocx.lock` — the child `task verify` and the acceptance suite run.
  Re-lock with the `ocx` named in the lock's own `generated_by`, never the newest:
  Bazel reads `ocx.lock` through the `ocx` that `rules_ocx` bootstraps (0.6.2 at
  rules_ocx 0.5.0), which refuses the `lock_version = 4` an `ocx` from 0.6.6 on
  writes. The `lock_version` line must not appear in the diff
- `OCX_CONTAINER_CLI_TAG` in `src/command/package/pipeline/generate/ci/matrix.rs`
  — the `ocx` every generated workflow's test legs install; the `tests/golden/`
  diff must be that one `version:` line per `setup-ocx` step and nothing else
- every `ocx-sh/setup-ocx` `version:` step under `.github/`
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
tags), then creates the tag and a GitHub Release; mode `candidate` ships `X.Y.Z-rcN` as a pre-release, and
mode `dev` publishes `ghcr.io/ocx-sh/dev/mirror` from the dispatched branch.

## License

Apache-2.0 — see [LICENSE](./LICENSE).
