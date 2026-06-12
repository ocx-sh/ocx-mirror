# CLI Reference

`ocx-mirror` mirrors upstream binary releases into OCI registries. Every command takes a [`mirror.yml`][ref-mirror-yml] spec; `sync`, `check`, and `validate` form the local loop, while the `pipeline` family implements the generated CI pipeline job by job.

## Global flags {#global-flags}

| Flag | Values | Description |
|------|--------|-------------|
| `--log-level <LEVEL>` | `trace`, `debug`, `info`, `warn`, `error` | Log verbosity (default: `info`) |
| `--color <WHEN>` | `auto`, `always`, `never` | When to use ANSI colors in output (default: `auto`) |

## `sync` {#sync}

Mirror packages from a spec file to an OCI registry: list upstream versions, resolve assets per platform, filter against tags already published, then download, verify, bundle (concurrent), and push (sequential by version, oldest first).

```sh
ocx-mirror sync <SPEC> [OPTIONS]
```

| Argument / flag | Default | Description |
|-----------------|---------|-------------|
| `<SPEC>` | — | Path to the mirror spec YAML file |
| `--work-dir <DIR>` | `./.ocx-mirror` | Working directory for downloads, bundles, and intermediate artifacts. Persists between runs so failed tasks resume without re-downloading; cleaned up per task after a successful push. |
| `--dry-run` | off | Only check what would be mirrored |
| `--version <V>` | — | Only mirror specific versions. Comma-separated or repeated (`--version 3.28.0,3.29.0`). Matched against the version string extracted from the source. |
| `--latest` | off | Only mirror the highest version. Applied after all other filters. |
| `--fail-fast` | off | Stop on first failure instead of continuing |
| `--format <FMT>` | `plain` | Output format: `plain` (table + summary) or `json` |

## `check` {#check}

Dry-run alias for [`sync`](#sync): identical discovery and filtering, no downloads, no pushes. Accepts the same arguments and flags as `sync` (`--dry-run` is forced on).

```sh
ocx-mirror check <SPEC> [OPTIONS]
```

## `validate` {#validate}

Validate a mirror spec file — YAML schema, regex syntax, required capture groups. No network access.

```sh
ocx-mirror validate <SPEC>
```

## `schema` {#schema}

Generate a JSON Schema for mirror types and print it to stdout.

```sh
ocx-mirror schema <TARGET>
```

| Argument | Values | Description |
|----------|--------|-------------|
| `<TARGET>` | `url-index` | Schema to generate (the `url_index` source document format) |

## `pipeline` {#pipeline}

Subcommands implementing the per-mirror CI pipeline. Each maps to one job in the workflow rendered by [`pipeline generate ci`](#pipeline-generate-ci): discover → prepare → test → push → notify. The test job runs `ocx package test` directly; everything else is an `ocx-mirror` invocation.

### `pipeline generate ci` {#pipeline-generate-ci}

Render (or check) the CI workflow files for a mirror repository. Writes `.github/workflows/mirror.yml`, `describe.yml`, and — unless the spec sets `allow_manual_edits: true` — the `verify-generated.yml` drift guard.

```sh
ocx-mirror pipeline generate ci [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--check` | off | Verify generated files are up to date; exit 65 on drift |
| `--format <FMT>` | — | Output format for diagnostics (`plain`, `json`) |

Rendering is idempotent. Specs with hardcoded webhook URLs, an empty `tests:` list, or `containers:` blocks (not supported by the current native-only renderer) are rejected with exit 64 before any file is written.

### `pipeline plan` {#pipeline-plan}

Compute which versions need work. Side-effect-free: queries the upstream source and the target registry, then emits a plan document listing versions to mirror, including the resolved per-platform asset URLs.

```sh
ocx-mirror pipeline plan [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--format <FMT>` | auto | `plain` or `json`. Without the flag, JSON is selected automatically when `GITHUB_ACTIONS=true`. |

### `pipeline prepare` {#pipeline-prepare}

Download, verify, and bundle one version across all declared platforms. Writes `{work_dir}/{V}/{platform_slug}/bundle.tar.xz` per platform plus `{work_dir}/{V}/manifest.json` with sizes and digests.

```sh
ocx-mirror pipeline prepare --version <V> [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--version <V>` | required | Version to prepare (e.g. `3.29.0`) |
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--work-dir <DIR>` | `./.ocx-mirror` | Working directory for intermediate artifacts |
| `--plan <PATH>` | — | A `plan.json` produced by [`pipeline plan`](#pipeline-plan). When set, tasks are built from the plan's resolved assets and the source is never queried — one crawl per pipeline run instead of one per prepare leg. |

### `pipeline push` {#pipeline-push}

Aggregate JUnit results and publish passing platform packages. Single serial push driver and the sole writer of cascade tags in the pipeline: for each `(version, platform)` pair, all containers must be green for the bundle to publish.

```sh
ocx-mirror pipeline push --bundles-dir <DIR> --junit-dir <DIR> --write-summary <PATH> [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--bundles-dir <DIR>` | required | Directory containing `bundle-{V}-{platform_slug}.tar.xz` files |
| `--junit-dir <DIR>` | required | Directory containing `junit-{V}-{platform_slug}-{container_id}.xml` files |
| `--write-summary <PATH>` | required | Path to write the `run-summary.json` output file |

Exits 0 even when some versions fail — the summary records per-version outcomes. Exits 69 on registry unreachability mid-push, 74 on I/O failure reading JUnit/bundles or writing the summary.

### `pipeline notify` {#pipeline-notify}

Post a [Discord][discord] webhook notification from `run-summary.json`. Silent (exit 0, no POST) when all versions were skipped as already existing and no test failures occurred. Reads the webhook URL from [`OCX_MIRROR_DISCORD_HOOK`][env-discord-hook] and the optional mention target from [`OCX_MIRROR_DISCORD_USER_ID`][env-discord-user-id].

```sh
ocx-mirror pipeline notify --run-summary <PATH>
```

| Flag | Default | Description |
|------|---------|-------------|
| `--run-summary <PATH>` | required | Path to the `run-summary.json` produced by [`pipeline push`](#pipeline-push) |

### `pipeline describe` {#pipeline-describe}

Publish catalog metadata (README + logo) to the registry by spawning `ocx package describe`. Reads the `catalog:` spec section; when the resolved README (default `CATALOG.md`) does not exist, the command logs and exits 0.

```sh
ocx-mirror pipeline describe [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |

## Exit codes {#exit-codes}

Codes align with BSD `sysexits.h`, shared with the `ocx` CLI.

| Code | Meaning | Raised by |
|------|---------|-----------|
| 0 | Success | — |
| 1 | Pipeline execution failure (download, push, verify) | `sync`, `prepare`, `push` |
| 64 | Usage error: hardcoded webhook URL, empty `tests:`, `containers:` blocks, ambiguous shell | `validate`, `pipeline generate ci` |
| 65 | Data error: spec validation failed, renderer drift (`--check`), JUnit/plan/run-summary malformed | all |
| 69 | Upstream source or target registry unreachable; Discord 5xx / timeout | `sync`, `check`, `plan`, `push`, `notify` |
| 74 | I/O error: template render or file write failure | `pipeline generate ci`, `push` |
| 77 | Discord 401/403 — webhook secret likely rotated | `pipeline notify` |
| 79 | Spec file not found | all |

<!-- external -->
[discord]: https://discord.com/developers/docs/resources/webhook

<!-- internal -->
[ref-mirror-yml]: ./mirror-yml.md
[env-discord-hook]: ./environment.md#ocx-mirror-discord-hook
[env-discord-user-id]: ./environment.md#ocx-mirror-discord-user-id
