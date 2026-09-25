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
  and `MODULE.bazel`'s `rust.toolchain(versions = [...])` with that channel
  (`task bazel:pin:check` reds on a mismatch)
- keep the dependency feature lists in root `Cargo.toml`'s
  `[workspace.dependencies]` in sync with ocx's `[workspace.dependencies]`
- the `[patch.crates-io]` table must keep pointing at the nested fork
  submodules (`external/ocx/external/...`) — see the comment in `Cargo.toml`
  (`task bazel:patch:check` asserts the same binding in `Cargo.bazel.lock.json`)
- keep the Bazel pins copied from ocx in sync: `rules_rust`, `rules_shell`,
  `buildifier_prebuilt` and `rules_ocx` `bazel_dep` versions, and
  `.bazelversion` / `ocx.toml`'s bazel tag against `external/ocx/MODULE.bazel`
  and `external/ocx/.bazelversion`
  (`git -C "$(git rev-parse --show-toplevel)/external/ocx" diff $OLD..HEAD -- MODULE.bazel .bazelversion ocx.toml`)

When the bump raises the `ocx` floor (a new subcommand or flag the mirror
spawns), the pinned `ocx` moves with it, in four places:

- `ocx.toml` / `ocx.lock` — `ocx update`, so `task verify` runs the child the
  code expects
- `OCX_CONTAINER_CLI_TAG` in `src/command/package/pipeline/generate/ci/matrix.rs`
  (the `setup-ocx` version every generated workflow bakes in), then regenerate
  the golden fixtures under `tests/golden/`
- the `setup-ocx` `version:` steps in `.github/workflows/verify.yml`
  (acceptance tests, Bazel graph) and `.github/workflows/oci-publish.yml`
- the floor prose in `.claude/rules/subsystem-mirror.md` and
  `docs/reference/environment.md`

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
                       # refresh MODULE.bazel.lock, update CHANGELOG, verify
# review, then:
git add -A && git commit -m "release: vX.Y.Z"
git tag vX.Y.Z
git push --atomic origin main vX.Y.Z
```

Tag push publishes `ocx.sh/ocx/mirror:<version>_<timestamp>` (+ cascade tags)
and a GitHub Release. Rolling dev builds publish to `dev.ocx.sh/ocx/mirror`
via the manually dispatched **Deploy Dev** workflow.

## License

Apache-2.0 — see [LICENSE](./LICENSE).
