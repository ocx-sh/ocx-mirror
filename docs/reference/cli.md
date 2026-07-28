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

Subcommands implementing the per-mirror CI pipeline. Each maps to one job in the workflow rendered by [`pipeline generate ci`](#pipeline-generate-ci): discover → prepare → test → push → notify. `describe`, `announce` and `patch` each own a standalone workflow outside that chain; the `patch` and `announce` ones are `workflow_dispatch` only, because both act on an already-published mirror on a maintainer's decision. The test job runs `ocx package test` directly; everything else is an `ocx-mirror` invocation.

### `package pipeline generate ci` {#pipeline-generate-ci}

Render (or check) the CI workflow files for a mirror repository. A repository may hold several mirror specs — `--spec` repeats, once per spec (see [Multi-spec repositories][ref-multi-spec]). Each spec gets its own `mirror.yml` / `describe.yml` / `patch.yml` set, plus `announce-from-registry.yml` when it has an [`announce:`][spec-announce] block; the repository gets exactly one `verify-generated.yml` drift guard, unless every spec sets `allow_manual_edits: true`. Generated filenames derive from where each spec sits relative to the repository root: the root spec keeps today's names byte for byte, any other spec gets every name suffixed with its own directory.

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

Alongside `new` (not yet published) and `backfill-partial` (published for some platforms, missing for others), a plan entry can carry kind `metadata-drift`: a published `(version, platform)` whose config blob no longer matches what the spec would publish today. Drift is only ever reported, never acted on — a version already scheduled as `new` or `backfill-partial` is never also reported as drifted, since its next push writes current metadata anyway.

The JSON document is `schema_version: 3` and adds a `has_drift` flag alongside `has_new`:

```json
{
  "schema_version": 3,
  "has_new": true,
  "has_drift": false,
  "versions": [...],
  "target": "ocx.sh/cmake",
  "ocx_mirror_rev": "abc123..."
}
```

`has_new` deliberately ignores drift-only versions — the generated workflow's `discover` job gates the download-and-build jobs on it, and a drift fix has nothing to download. The `discover` job also drops every `metadata-drift` entry before building the `prepare` matrix: a drift entry carries no resolved assets, so a `prepare` leg for one would abort looking for a bundle nothing wrote. `has_drift` surfaces the finding for a human to act on with [`pipeline patch`](#pipeline-patch).

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

### `package pipeline patch` {#pipeline-patch}

Correct the published metadata of versions the registry already holds, without re-downloading or re-uploading anything. Package metadata lives in the OCI config blob, never in a layer, so a fix is a manifest re-emission that re-references the existing layers by digest — the only bytes uploaded are a config blob the size of `metadata.json`. This is the retroactive counterpart to a `metadata-drift` entry in [`pipeline plan`](#pipeline-plan): fix `metadata.json` (or the spec's `metadata:` block) in the mirror repo, then run `patch` to reach every version already published under the old, wrong metadata — the alternative, deleting tags and re-mirroring, costs hours of upstream download and orphans anyone pinned to a digest.

Runs from the generated `patch.yml` workflow, which is `workflow_dispatch` only — never scheduled, and deliberately not wired to `pipeline plan`'s `has_drift` output. Whether a drift finding is worth re-emitting manifests over is a maintainer's decision; the workflow exists so that acting on it does not need registry push credentials and an index token on somebody's laptop. Dispatch it from the repository's **Actions** tab, or:

```sh
gh workflow run patch.yml --repo <owner>/<mirror> -f version=3.29.0
```

Its three inputs are this command's selection flags. `version` takes one or several versions, separated by spaces or commas, and becomes one `--version` flag each; `min_version` and `max_version` pass through. An input left empty contributes no flag at all, so dispatching with every field blank patches every published version.

```sh
ocx-mirror package pipeline patch --metadata-only [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `--spec <PATH>` | `./mirror.yml` | Path to the mirror spec file |
| `--metadata-only` | — | Required. Republish the metadata blob and nothing else — currently the only mode. |
| `--version <VERSION>` | — | Patch this published version. Repeatable. Matches the leaf tag verbatim or by its version core, so `--version 3.29.0` selects `3.29.0_20260610` on a build-stamped mirror. Composes with `--min-version`/`--max-version` as a union. A version the registry does not publish is a usage error (exit 64), not a silent no-op. |
| `--min-version <VERSION>` | — | Lowest published version to patch, inclusive. Compared on the version core, so it covers every build stamp of the version it names. |
| `--max-version <VERSION>` | — | Highest published version to patch, exclusive. Compared on the version core, so it excludes every build stamp of the version it names. |

Omitting `--version`, `--min-version`, and `--max-version` all at once patches every published version.

**Only leaf tags are patched.** A cascade alias (`3.29`, `3.29.0` on a build-stamped mirror, `latest`) shares its leaf's child manifests and re-cascades from it automatically once the leaf is patched, so naming an alias in `--version` is a usage error rather than a silent no-op or a wasted duplicate run.

**Idempotent.** A `(version, platform)` whose published config blob already matches what the spec would publish today is skipped — settled from the config descriptor's digest, with no registry blob fetch for the common case. There is no ledger and no stored range: the comparison against the currently published bytes is the entire mechanism, which is also why the version range lives only on this command line and not in `mirror.yml` — a stored range would need a ledger to know what it had already covered.

**Announces on success.** A run that republished anything ends by announcing (the same `--tags-from-registry` path as [`pipeline announce`](#pipeline-announce)), because the re-emitted manifests are live under digests the index does not yet know. An announce that fails after a successful patch fails the command loudly: that is the state where the index still points at digests the patch just replaced. An *absent* [`OCX_ANNOUNCE_TOKEN`][env-announce-token] does not fail it — a repository without the secret is a valid configuration, and the skip is recorded as a notice, exactly as in [`pipeline push`](#pipeline-push).

**Layer pins are never disturbed.** Layer digests are unchanged by a metadata patch, so no layer is ever orphaned; the patched manifest gets its own canonical `sha256:<hex>` tag alongside the version tag.

## Exit codes {#exit-codes}

Codes align with BSD `sysexits.h`, shared with the `ocx` CLI.

| Code | Meaning | Raised by |
|------|---------|-----------|
| 0 | Success | — |
| 1 | Pipeline execution failure (download, push, verify, republish, or a failed post-patch announce) | `sync`, `prepare`, `push`, `pipeline patch` |
| 64 | Usage error: hardcoded webhook URL, empty `tests:`, ambiguous shell, no `announce:` block, two specs sharing one directory, a spec outside `--repo-root`, an unparseable `--version`/`--min-version`/`--max-version`, a `--version` the registry does not publish or that names a cascade alias | `validate`, `pipeline generate ci`, `pipeline announce`, `pipeline patch` |
| 65 | Data error: spec validation failed, renderer drift (`--check`) — including a generated workflow left behind by a spec dropped from `--spec` — JUnit/plan/run-summary malformed | all |
| 69 | Upstream source or target registry unreachable; Discord 5xx / timeout | `sync`, `check`, `plan`, `push`, `notify`, `pipeline patch` |
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
