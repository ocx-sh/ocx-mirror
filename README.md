# ocx-mirror

Mirror upstream tool releases (GitHub Releases, URL indexes) into any OCI
registry as [OCX](https://github.com/ocx-sh/ocx) packages. YAML-configured,
two-phase pipeline (concurrent prepare, sequential push with cascade tagging),
generated GitHub Actions CI pipelines with per-platform smoke tests.

- **Documentation**: <https://ocx-sh.github.io/ocx-mirror/>
- **Install**: `ocx --global add ocx.sh/ocx/mirror`

## Development

This repository vendors ocx as a git submodule (`external/ocx`) and consumes
`ocx_lib` as a path dependency into it — clone recursively:

```sh
git clone --recurse-submodules https://github.com/ocx-sh/ocx-mirror.git
```

Toolchain bootstraps via [direnv](https://direnv.net) + `ocx direnv export`
(see `ocx.toml`). Common tasks:

```sh
task            # fast check (fmt, clippy, cargo check)
task verify     # full gate (lint, licenses, build, unit + acceptance tests)
task test       # acceptance tests (needs Docker for the local registry)
```

## Bumping ocx_lib (the `external/ocx` submodule)

`ocx_lib` is not a published crate — its version is whatever the submodule
points at. To advance:

```sh
git -C external/ocx fetch origin && git -C external/ocx checkout origin/main
git -C external/ocx submodule update --init --recursive   # nested fork submodules
cargo update -p ocx_lib                                   # if the version changed
task verify
git add external/ocx Cargo.lock && git commit -m "chore(deps): bump external/ocx"
```

Checklist when bumping:

- keep `rust-toolchain.toml` channel in sync with `external/ocx/rust-toolchain.toml`
- keep the dependency feature lists in `Cargo.toml` in sync with ocx's
  `[workspace.dependencies]`
- the `[patch.crates-io]` table must keep pointing at the nested fork
  submodules (`external/ocx/external/...`) — see the comment in `Cargo.toml`

## Mirror development against unreleased ocx changes

Point the submodule at an ocx feature branch, develop, and land the ocx side
first; then bump the submodule here to the landed commit.

## Releases

```sh
task release:prepare   # compute version (git-cliff), update Cargo.toml + CHANGELOG, verify
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
