# CLI Reference

`ocx-mirror` mirrors upstream binary releases into OCI registries. Package-mirroring commands live under the `package` namespace and take a [`mirror.yml`][ref-mirror-yml] spec: `package sync`, `package check`, and `package validate` form the local loop, while the `package pipeline` family implements the generated CI pipeline job by job. `schema` is a top-level utility. (A sibling `registry` namespace for registry-to-registry mirroring is reserved for a future release.)

## Global flags {#global-flags}

| Flag | Values | Description |
|------|--------|-------------|
| `--log-level <LEVEL>` | `trace`, `debug`, `info`, `warn`, `error` | Log verbosity (default: `info`) |
| `--color <WHEN>` | `auto`, `always`, `never` | When to use ANSI colors in output (default: `auto`) |

## `package sync` {#sync}

Mirror packages from a spec file to an OCI registry: list upstream versions, resolve assets per platform, filter against tags already published, then download, verify, bundle (concurrent), and push (sequential by version, oldest first).

```sh
ocx-mirror package sync <SPEC> [OPTIONS]
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

## `package check` {#check}

Dry-run alias for [`package sync`](#sync): identical discovery and filtering, no downloads, no pushes. Accepts the same arguments and flags as `package sync` (`--dry-run` is forced on).

```sh
ocx-mirror package check <SPEC> [OPTIONS]
```

## `package validate` {#validate}

Validate a mirror spec file — YAML schema, regex syntax, required capture groups. No network access.

```sh
ocx-mirror package validate <SPEC>
```

## `schema` {#schema}

Generate a JSON Schema for mirror types and print it to stdout.

```sh
ocx-mirror schema <TARGET>
```

| Argument | Values | Description |
|----------|--------|-------------|
| `<TARGET>` | `url-index` | Schema to generate (the `url_index` source document format) |

## `package pipeline` {#pipeline}

Subcommands implementing the per-mirror CI pipeline. Each maps to one job in the workflow rendered by [`pipeline generate ci`](#pipeline-generate-ci): discover → prepare → test → push → notify. `describe` and `announce` each own a standalone workflow outside that chain. The test job runs `ocx package test` directly; everything else is an `ocx-mirror` invocation.

### `package pipeline generate ci` {#pipeline-generate-ci}

Render (or check) the CI workflow files for a mirror repository. A repository may hold several mirror specs — `--spec` repeats, once per spec (see [Multi-spec repositories][ref-multi-spec]). Each spec gets its own `mirror.yml` / `describe.yml` pair, plus `announce-from-registry.yml` when it has an [`announce:`][spec-announce] block; the repository gets exactly one `verify-generated.yml` drift guard, unless every spec sets `allow_manual_edits: true`. Generated filenames derive from where each spec sits relative to the repository root: the root spec keeps today's names byte for byte, any other spec gets every name suffixed with its own directory.

```sh
ocx-mirror package pipeline generate ci [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to a mirror spec file; repeat once per spec the repository holds |
| `--repo-root <DIR>` | the directory every `--spec` shares | Repository root the workflows are written under, and generated filenames are computed relative to |
| `--check` | off | Verify generated files are up to date; exit 65 on drift |
| `--format <FMT>` | — | Output format for diagnostics (`plain`, `json`) |

Rendering is idempotent, and does not depend on the order repeated `--spec` flags are given in. Specs with hardcoded webhook URLs or an empty `tests:` list are rejected with exit 64 before any file is written — as are two specs sharing one directory, and a spec that does not resolve under `--repo-root` (see [Multi-spec repositories][ref-multi-spec]).

### `package pipeline plan` {#pipeline-plan}

Compute which versions need work. Side-effect-free: queries the upstream source and the target registry, then emits a plan document listing versions to mirror, including the resolved per-platform asset URLs.

```sh
ocx-mirror package pipeline plan [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--format <FMT>` | auto | `plain` or `json`. Without the flag, JSON is selected automatically when `GITHUB_ACTIONS=true`. |

### `package pipeline prepare` {#pipeline-prepare}

Download, verify, and bundle one version across all declared platforms. Writes `{work_dir}/{V}/{platform_slug}/bundle.tar.xz` per platform plus `{work_dir}/{V}/manifest.json` with sizes and digests.

```sh
ocx-mirror package pipeline prepare --version <V> [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--version <V>` | required | Version to prepare (e.g. `3.29.0`) |
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--work-dir <DIR>` | `./.ocx-mirror` | Working directory for intermediate artifacts |
| `--plan <PATH>` | — | A `plan.json` produced by [`pipeline plan`](#pipeline-plan). When set, tasks are built from the plan's resolved assets and the source is never queried — one crawl per pipeline run instead of one per prepare leg. |

### `package pipeline push` {#pipeline-push}

Aggregate JUnit results and publish passing platform packages. Single serial push driver and the sole writer of cascade tags in the pipeline: for each `(version, platform)` pair, all containers must be green for the bundle to publish.

```sh
ocx-mirror package pipeline push --bundles-dir <DIR> --junit-dir <DIR> --write-summary <PATH> [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--bundles-dir <DIR>` | required | Directory containing `bundle-{V}-{platform_slug}.tar.xz` files |
| `--junit-dir <DIR>` | required | Directory containing `junit-{V}-{platform_slug}-{container_id}.xml` files |
| `--write-summary <PATH>` | required | Path to write the `run-summary.json` output file |

Exits 0 even when some versions fail — the summary records per-version outcomes. Exits 69 on registry unreachability mid-push, 74 on I/O failure reading JUnit/bundles or writing the summary.

### `package pipeline notify` {#pipeline-notify}

Post a [Discord][discord] webhook notification from `run-summary.json`. Silent (exit 0, no POST) when all versions were skipped as already existing and no test failures occurred. Reads the webhook URL from [`OCX_MIRROR_DISCORD_HOOK`][env-discord-hook] and the optional mention target from [`OCX_MIRROR_DISCORD_USER_ID`][env-discord-user-id].

```sh
ocx-mirror package pipeline notify --run-summary <PATH>
```

| Flag | Default | Description |
|------|---------|-------------|
| `--run-summary <PATH>` | required | Path to the `run-summary.json` produced by [`pipeline push`](#pipeline-push) |

### `package pipeline describe` {#pipeline-describe}

Publish catalog metadata (README + logo) to the registry by spawning `ocx package describe`. Reads the `catalog:` spec section; when the resolved README (default `CATALOG.md`) does not exist, the command logs and exits 0.

```sh
ocx-mirror package pipeline describe [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |

### `package pipeline announce` {#pipeline-announce}

Announce every tag the target repository currently holds into the index, by spawning `ocx package announce --tags-from-registry`. Additive: it cannot drop a tag the index already commits, and yank markers survive.

This is the catch-up path for a mirror that published before it gained an [`announce:`][spec-announce] block — the push job announces only what its own run wrote, so no future run ever covers that backlog. Driven by the generated `announce-from-registry.yml` workflow, which is `workflow_dispatch` only.

```sh
ocx-mirror package pipeline announce [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--dry-run` | off | Write the rebuilt entry to a temporary directory and report `updated` / `unchanged` without opening a pull request |

Needs [`OCX_ANNOUNCE_TOKEN`][env-announce-token] unless `--dry-run` is set. Exits 64 when the spec has no `announce:` block — there is no index package to announce into.

## Exit codes {#exit-codes}

Codes align with BSD `sysexits.h`, shared with the `ocx` CLI.

| Code | Meaning | Raised by |
|------|---------|-----------|
| 0 | Success | — |
| 1 | Pipeline execution failure (download, push, verify) | `sync`, `prepare`, `push` |
| 64 | Usage error: hardcoded webhook URL, empty `tests:`, ambiguous shell, no `announce:` block, two specs sharing one directory, a spec outside `--repo-root` | `validate`, `pipeline generate ci`, `pipeline announce` |
| 65 | Data error: spec validation failed, renderer drift (`--check`) — including a generated workflow left behind by a spec dropped from `--spec` — JUnit/plan/run-summary malformed | all |
| 69 | Upstream source or target registry unreachable; Discord 5xx / timeout | `sync`, `check`, `plan`, `push`, `notify` |
| 74 | I/O error: template render or file write failure | `pipeline generate ci`, `push` |
| 77 | Discord 401/403 — webhook secret likely rotated | `pipeline notify` |
| 79 | Spec file not found | all |

<!-- external -->
[discord]: https://discord.com/developers/docs/resources/webhook

<!-- internal -->
[ref-mirror-yml]: ./mirror-yml.md
[ref-multi-spec]: ./mirror-yml.md#multi-spec
[spec-announce]: ./mirror-yml.md#announce
[env-discord-hook]: ./environment.md#ocx-mirror-discord-hook
[env-discord-user-id]: ./environment.md#ocx-mirror-discord-user-id
[env-announce-token]: ./environment.md#ocx-announce-token
