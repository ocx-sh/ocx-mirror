# WP3 — ocx 0.6.0 adoption: CI, docs, harness

Notes for the team lead. Scope was `.github/workflows/**`, `docs/**` (except
`mirror-yml.md`), `test/**` (except `test/bin/ocx-mirror`), `.envrc`,
`ocx.toml`, `Taskfile.yml`, `taskfiles/**`, `README.md`, `CLAUDE.md`,
`.claude/rules/**`. Nothing committed, nothing pushed.

## Remaining action items

- **Bump `OCX_CONTAINER_CLI_TAG` in
  `src/command/package/pipeline/generate/ci/matrix.rs:302` from `v0.5.8` to
  `v0.6.0`** — still stale as of this writing. One constant drives *both*
  generated version knobs (see Q1 below), so at `v0.5.8` a fleet repo without
  its own `cli` binding bootstraps an ocx that rejects `--tags-file` with exit
  64, and every container test leg curls a 0.5.8 binary regardless. The line
  carries a `renovate:` anchor — keep the literal on one line or the
  customManager regex stops matching. Out of my scope (`src/**`).

- **Rename `src/command/package/pipeline/describe.rs:238`** (and the expected
  argv at `:290`) from `["package", "describe", …]` to
  `["package", "description", "push", …]`. `describe` is a hidden deprecated
  alias in 0.6 that is deleted in 0.7. `docs/reference/cli.md` and
  `docs/reference/environment.md` already document `description push`, so the
  docs lead the code until this lands. Owner: WP2-cli-verbs.

- **Decide on the `ocx.lock` refresh.** Bumping `ocx.toml` re-resolved all five
  declared tags, not just `ocx` — see Q2. `ocx lock` cannot re-resolve a single
  binding (`external/ocx/crates/ocx_cli/src/command/lock.rs:22-24`: "When the
  config drifted, every declared tag is re-resolved"). Leaving `ocx.toml` at
  0.5.8 is not an option: the mirror resolves its child `ocx` from `PATH`, which
  locally *is* this pin, so the acceptance suite would exit 64 on
  `--tags-file`.

- **Free host port 5001, or accept silent cross-repo registry reuse.**
  `test/src/helpers.py:48-49` (`start_registry`) returns early when the address
  is already reachable, so it never reaches `docker compose up`. With the
  sibling `ocx` repo's `test-mirror-registry-1` holding 5001,
  `task test:parallel` **does not fail — it runs the whole suite against that
  other project's registry**. The dedicated `name:` + port in the compose header
  prevents a *clash*; nothing detects a *squatter*. I left the `test` project
  running as instructed. Ownership assertion:
  `docker inspect $(docker ps -q --filter publish=5001) --format '{{index .Config.Labels "com.docker.compose.project"}}'`
  must print `ocx-mirror-test`.

## Q1 — which ocx executes `package announce` / `package push`

Three knobs, but only two live in this repo, and they share one constant.

| Knob | file:line | Current value |
|---|---|---|
| setup-ocx bootstrap `with: version:` | `templates/workflow.yml:179` (also `:29`, `:74`, `:107`, `:275`), `announce-from-registry.yml:39`, `cascade.yml:34`, `describe.yml:25`, `patch.yml:32`, `verify-generated.yml:31` — every one is `{OCX_CLI_VERSION}` | `0.5.8` |
| container-leg curl'd ocx | `generate/ci/matrix.rs:394`, `{OCX_CLI_TAG}` in the `releases/download/` URL | `v0.5.8` |
| project-toolchain `cli` binding | the **downstream** repo's own `ocx.toml` — not templated from here | fleet's `cli:0.5` |

Both templated knobs resolve from the same constant:
`generate/ci/matrix.rs:302` `OCX_CONTAINER_CLI_TAG = "v0.5.8"`, with
`ocx_cli_version()` at `:308-310` stripping the leading `v` for setup-ocx.

**Answer: the project-toolchain `cli` binding is what executes push and
announce.** The generated push step calls `ocx-mirror` directly, never through
`ocx exec` — `templates/workflow.yml:208-213` spells out why (wrapping it would
set `OCX_BINARY_PIN` to the *bootstrap* ocx, and the nested
`ocx package push --format json` would then resolve that older binary, emit no
JSON report, and be misrecorded as a red). `ocx-mirror` therefore resolves its
child `ocx` from `PATH`, and setup-ocx has already activated the project:
its own `action.yml` documents input `project` as "When the file exists, the
action runs `ocx pull` and activates the project so subsequent steps can invoke
the bound tools directly." The bound `cli` shadows the bootstrap on `PATH`.

**So the fleet sweep needs both, for different reasons.** `cli:0.6` is what
actually runs and is sufficient for any mirror repo that declares the binding.
The bootstrap bump still matters for two cases `cli` does not cover: (a) a
mirror repo with no `cli` binding, where the bootstrap ocx *is* the `PATH` ocx;
(b) the container test legs, which curl `OCX_CONTAINER_CLI_TAG` directly and
ignore `PATH` entirely.

## Q2 — yes, all four moved

All five digests changed; none re-resolved to the same manifest. Versions
resolved from the newly materialised packages:

| Binding | Tag | Now |
|---|---|---|
| go-task | `ocx.sh/go-task/task:3` | task 3.53.1 |
| git-cliff | `ocx.sh/git-cliff/git-cliff:2` | git-cliff 2.13.1 |
| uv | `ocx.sh/astral-sh/uv:0` | uv 0.12.5 |
| lychee | `ocx.sh/lychee/lychee:0` | lychee 0.24.2 |
| ocx | `ocx.sh/ocx/cli:0.6.0` | ocx 0.6.0 (commit `e48ef73c`, = the submodule pin) |

The old lock read `generated_by = "ocx 0.5.6"`,
`generated_at = "2026-08-13T23:38:14Z"` — 18 days stale against four *floating
major* tags. So this is 18 days of ordinary upstream releases surfacing at once,
not churn this change introduced. Structure is unchanged (uv keeps both
`+libc.glibc` platform keys, 2 before and 2 after).

Suggested release-commit wording: "relock: ocx 0.5.8 → 0.6.0; the four floating
tags advance with it (task 3.53.1, git-cliff 2.13.1, uv 0.12.5, lychee 0.24.2)
because `ocx lock` re-resolves every declared tag once the config drifts."

## Q3 — exact `0.6.0` is deliberate here; do not float it

Pre-change value, verbatim from `ocx.toml`:

```toml
ocx = "ocx.sh/ocx/cli:0.5.8"
```

An exact patch pin, and the **only** one in the file — the other four bindings
are floating majors (`go-task:3`, `git-cliff:2`, `uv:0`, `lychee:0`). That
asymmetry is the existing convention, and both workflows state the reason:
"Kept level with the `external/ocx` submodule pin so the CLI that publishes a
release is the one the release was built against."

The submodule is pinned to an exact tag (`v0.6.0`), so a floating `:0.6` would
drift off it the moment 0.6.1 ships — which is exactly the invariant the pin
exists to hold. **Keep `0.6.0` exact here.** The fleet's floating `:0.6` is
correct *for the fleet*, which has no submodule to stay level with.
