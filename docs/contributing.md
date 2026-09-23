# Contributing

Clone recursively — ocx is vendored as the `external/ocx` git submodule and
its crates are path dependencies:

```sh
git clone --recurse-submodules https://github.com/ocx-sh/ocx-mirror.git
```

The toolchain comes from `ocx.toml` (direnv + `ocx direnv export`, or prefix
commands with `ocx exec --`). Tasks run through [Task](https://taskfile.dev):

| Command | What it does |
|---|---|
| `task` | Fast check: format, clippy, `cargo check` |
| `task rust:verify` | Loop gate: format, clippy, unit tests |
| `task verify` | Full gate: lint, licenses, release build, unit and acceptance tests; on Linux also the Bazel static gates and the coverage gate |
| `task test:parallel` | Acceptance suite under cargo (needs Docker for the local registry) |
| `task docs:serve` | This site, locally, from the pinned toolchain |
| `task docs:build` | This site as the Bazel target `//docs:site`, unpacked into `site/` |

## The Bazel loop (Linux)

On Linux the unit and acceptance tests run under [Bazel](https://bazel.build),
which caches results per target: an unchanged crate is not re-tested. macOS,
Windows, CI's test lanes and the release build stay on cargo.

| Command | What it does |
|---|---|
| `task bazel:bootstrap` | Once per fresh worktree: generate `Cargo.bazel.lock.json` |
| `task bazel:test:unit` | Unit tests; per-case JUnit at `target/bazel/junit.xml` |
| `task bazel:test:accept` | The acceptance suite as one cached test, against its own Sigstore stack from `external/ocx` (no registry or port overrides — use `task test:parallel` for those) |
| `task bazel:test:scoped` | Only the tests affected by what changed against `origin/main` |
| `task bazel:cache:gc` | Delete the disk cache when it exceeds `MAX_GB` (default 30); manual |

The site's toolchain is the hashed lock `docs/requirements.lock`, compiled from
`docs/requirements.in` by the command in its header. Bazel builds the site
offline and sandboxed from that lock, so a CI run can reuse the cached site.

Bazel is `ocx exec bazel -- bazel`, pinned in `ocx.toml`. Tasks pass
`--jobs=6`; set `JOBS=` to change it. `.bazelrc` caps the server heap at 2 GB
and actions at 30% of host RAM — run one Bazel server per worktree and
`bazel shutdown` when done. If `~/.cargo/config.toml` sits above `/tmp`, a
manual repin needs `--repo_env=TMPDIR=/var/tmp/ocx-mirror-splice`
(`bazel:bootstrap` passes it).

## Test telemetry

Test and build timings can be pushed to `otel.ocx.sh`. Locally this is off
unless `~/.config/ocx-telemetry/env` names an endpoint; setup is described in
the header of `taskfiles/telemetry.taskfile.yml`. A telemetry failure is a
warning, never a failed task. In CI the `OTEL_OTLP_AUTH` secret enables it; fork
pull requests push nothing.
