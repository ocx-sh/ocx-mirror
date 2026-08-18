---
paths:
  - src/**
  - tests/**
  - crates/**
---

# Mirror Subsystem

Separate crate (`ocx_mirror`) mirror upstream tool releases to OCI registries. YAML-configured, two-phase pipeline.

## Design Rationale

Separate crate: mirror tool standalone binary, own CLI, not part of `ocx` package manager. Two-phase pipeline (prepare concurrent, push sequential) ensure cascade tag order correct — tags push in semver order so `latest` always point to highest version. Design-pattern catalog + Rust conventions: `quality-rust.md` (Design Patterns) + `quality-core.md` (SOLID/DRY/KISS/YAGNI); mirror-specific module/pipeline layout below.

## Module Map

| Path | Purpose |
|------|---------|
| `command.rs` | Top-level `Command` dispatcher: `Package` / `Registry` / `Dist` (subcommand groups) + `Schema`; threads printer + progress |
| `command/package/mod.rs` | `PackageCommand` dispatcher: routes sync/check/validate/pipeline |
| `command/registry/mod.rs` | `RegistryCommand` dispatcher for `ocx-mirror registry <verb>` — currently just `Sync`; sibling to `command/package`, wired into the top-level `Command::Registry` arm |
| `command/registry/sync.rs` | `registry sync` CLI verb (`Sync`): spec path positional (default `./registry.yml`) + `RegistrySyncOptions`; loads the spec, runs the sync, renders the report, maps the outcome to an exit code (C-045) |
| `command/registry/options.rs` | `RegistrySyncOptions` — shared `registry` verb flags: `--dry-run`, `--fail-fast`, `--repair-catalog`, `--cache-dir`, `--format`; `pub(crate)` because `pipeline::registry_sync` takes it directly, the same upward edge `command/package/pipeline` already carries for `pipeline::python_push` |
| `command/dist/mod.rs` | `DistCommand` dispatcher for `ocx-mirror dist <verb>` — currently just `Sync`; third sibling to `command/package` and `command/registry`, wired into the top-level `Command::Dist` arm. The only namespace that touches no OCI registry |
| `command/dist/sync.rs` | `dist sync` CLI verb (`Sync`): spec path positional (default `./dist.yml`) + `DistSyncOptions`; same load → run → report → classify shape as `registry sync` |
| `command/dist/options.rs` | `DistSyncOptions` — `--dry-run`, `--format` only. No `--cache-dir` (the destination is asked with a HEAD instead of a local ledger) and no `--fail-fast` (a failed archive already stops the run before anything is published) |
| `command/package/sync.rs` | Main sync command: spec → versions → filter → pipeline |
| `pipeline/target_registry.rs` | Fail-safe target-registry state loading (`list_target_tags`, `fetch_published_platforms`, `extract_platforms`) shared by sync + plan + push + patch + `python_push`; only authoritative not-found counts as absent (issue #157). At the pipeline layer, not under `command/`, so the env-push leg reaches it without an upward edge |
| `command/package/check.rs` | Dry-run sync |
| `command/package/validate.rs` | Spec validation only |
| `command/package/options.rs` | Shared `SyncOptions` (--exact-version, --latest, --fail-fast) |
| `command/package/pipeline/mod.rs` | `Pipeline` subcommand dispatcher; routes to generate/plan/prepare/push/notify |
| `command/package/pipeline/generate/mod.rs` | `generate` subgroup dispatcher |
| `command/package/pipeline/generate/ci.rs` | `pipeline generate ci` — `GenerateCi`, spec loading, render orchestration |
| `command/package/pipeline/generate/ci/matrix.rs` | Test-matrix legs, container wrapper, per-leg run steps; owns `workflow.yml`'s test loop |
| `command/package/pipeline/generate/ci/slot.rs` | `SpecSlot` — where a spec sits under the repo root, and the file names and trigger paths that follow |
| `command/package/pipeline/generate/ci/aux_workflows.rs` | `describe` / `announce-from-registry` / `patch` / `cascade` / `verify-generated` renderers and their templates |
| `command/package/pipeline/generate/ci/permissions.rs` | GHCR job scopes and registry login steps |
| `command/package/pipeline/generate/ci/drift.rs` | Writing generated files; the `--check` comparison, pin-normalised |
| `command/package/pipeline/plan.rs` | `pipeline plan` — discover new work, emit plan.json (schema v2: entries carry `source_version`, `variant`, resolved per-platform `assets`) |
| `command/package/pipeline/plan/env.rs` | `pylock`/`pypi` candidate selection, wheel constraints, per-version lock derivation |
| `command/package/pipeline/plan/drift.rs` | Published-metadata drift: config-digest short circuit, leaf versions, drift entries |
| `command/package/pipeline/prepare.rs` | `pipeline prepare --version V [--plan plan.json]` — download + bundle; `--plan` builds from discover's resolved assets, no source re-crawl (issue #160) |
| `command/package/pipeline/push.rs` | `pipeline push` — serial push driver, writes run-summary.json |
| `command/package/pipeline/push/alias.rs` | Rolling aliases and the backfill cascade repair (`cascade_backfilled_entries`, `re_cascade_entry`) |
| `command/package/pipeline/push/verdict.rs` | AND-across-containers JUnit evaluation and the resulting `VersionStatus` |
| `command/package/pipeline/push/gating.rs` | Which container legs gate a `+libc.*` platform entry |
| `command/package/pipeline/push/bundles.rs` | Bundle discovery and slug ↔ platform-key mapping |
| `command/package/pipeline/notify.rs` | `pipeline notify` — Discord webhook POST |
| `command/package/pipeline/cascade.rs` | `pipeline cascade` — wraps `ocx package cascade repair` (needs ocx ≥ 0.5.4), then announces the tags it moved |
| `spec.rs` | `MirrorSpec` root and its `impl`; shared regexes. Children glob re-exported `pub(crate)`, so callers keep saying `crate::spec::…` |
| `spec/validate.rs` | Every validation rule. Rejected documents are covered by `tests/fixtures/invalid/*.yml`, one file per rule |
| `spec/load.rs` | `load_spec()`, `extends:` chain resolution, shallow merge |
| `spec/platform_keys.rs` | Slug and `container_id` derivation — the join key between `prepare`'s bundle name, the renderer's JUnit name, and `push`'s lookup |
| `spec/source.rs` | `Source` enum (GithubRelease, UrlIndex, Pylock, Pypi) + `PackageIndex`; `pypi_indexes()` owns the pypi.org default. A `pypi` index URL carrying userinfo is refused at validation — credentials never live in a contributed spec, and the URL would reach `uv`'s argv |
| `spec/python_config.rs` | `PythonConfig` (interpreter version/ABI + `interpreter_package` ref, `lock` = pypi lock-derivation options, `entrypoints` = console-script synthesis mode) — required for `source.type: pylock`/`pypi` |
| `spec/target.rs` | `Target` (registry + repository) |
| `spec/assets.rs` | `AssetPatterns` (platform → regex[] mapping). Keys are `os/arch[/variant][+libc.<flavor>[,…]]` parsed via `ocx_lib` `Platform::from_str`; a `+libc.glibc`/`+libc.musl` suffix lands in `os_features` and publishes as an OCI `os.features` entry |
| `spec/asset_type.rs` | `AssetTypeConfig` (Archive vs Binary) |
| `spec/wheels.rs` | `WheelPatterns` (`wheels:` map, env sources only): platform key (optional single `+libc.<flavor>` os_feature) → ordered wheel-tag prefix filter (admissibility + ranking); `effective_filter` derives per-key defaults (`["any"]` plain linux, `["manylinux","any"]` glibc, `["musllinux","any"]` musl, macosx/win elsewhere); key published verbatim as the image-index platform entry; `base_platform_key`/`libc_feature` helpers |
| `spec/versions_config.rs` | Version filter (min/max bounds, new_per_run, backfill order) |
| `spec/cascade_config.rs` | `CascadeConfig` — `cascade:` as bool or `{schedule}` map (hand-rolled `Deserialize` visitor, so the map branch keeps its `unknown field` diagnostic; map implies enabled); `validate` charset-checks the cron |
| `spec/verify_config.rs` | Checksum verify options |
| `spec/metadata_config.rs` | Metadata.json path config |
| `spec/concurrency_config.rs` | Parallel download/bundle limits, source rate limiting, push retry count (`max_retries`) — no push-parallelism knob (`max_pushes` removed; a spec still setting it keeps parsing, ignored) |
| `spec/tests_config.rs` | `TestEntry` (name + command); top-level `tests:` schema |
| `spec/platforms_config.rs` | `PlatformConfig`, `ContainerConfig` (`image`/`shell`/`id`/`setup` — `setup` provisions the leg's image once per leg via `docker build`); `platforms:` matrix schema; per-platform version applicability (`min_version`/`max_version`/`exclude` of `ExcludeEntry`+`Severity`) |
| `spec/ocx_mirror_config.rs` | `OcxMirrorConfig` (`rev` only, `deny_unknown_fields`); pins nothing — reported as `ocx_mirror_rev` in `pipeline plan` |
| `spec/announce_config.rs` | `AnnounceConfig` (`package`, `fork`, `index_repo`, optional `schedule` putting the generated catch-up workflow on a timer — charset-checked by `validate_announce_config`); logical index name, spelled out — never derived from `target` |
| `spec/notify_config.rs` | `NotifyConfig`, `DiscordConfig` (`webhook_secret` + `user_id` snowflake); the URL-reject validator itself is `spec/validate.rs::policy_check_notify` |
| `spec/registry.rs` | `RegistrySpec` root (`registry.yml`) + `RegistrySource`, `RegistryConcurrency`, `OnError` — a different root type from `MirrorSpec`, not a variant of it (C-001…C-004, C-006); re-exported through `spec.rs` alongside `MirrorSpec`, `lib.rs` untouched (C-008) |
| `spec/dist.rs` | `DistSpec` root (`dist.yml`) + `Select`, `Publish`, `Upload`, `Identity` — the third root type, same tier and re-export shape as `RegistrySpec`. **Deliberate convention, third occurrence:** a spec root validates a grammar by calling into the pipeline module that owns it (here `dist_sync::layout::LayoutTemplate`, as `spec/registry.rs` calls `registry_sync::destination` and `catalog::index_host`). The owner of a grammar is the only place that can validate it; this inversion is accepted, not debt |
| `spec/prescan.rs` | `pre_scan()` — raw-`serde_yaml_ng::Value` scan of a merged spec document before typed deserialization: credential deny-list at any depth, the `kind:` discriminator (parameterised — `registry.yml` and `dist.yml` both pass their expected kind), `sources[].index` userinfo (C-005); every rejection is `SpecUsageError` (64), no offending value ever echoed. The deny-list is why `dist.yml` spells its upload credentials `identity:` with `*_env` names — a key called `auth` is refused at any depth, and the guard is worth more than the field name |
| `source/github_release.rs` | GitHub API client, tag pattern extraction |
| `source/url_index.rs` | JSON index fetch (remote, inline, generator) |
| `source/pylock.rs` | PEP 751 `pylock.toml` reader → single `VersionInfo` (the app's locked version, PEP 503 name match); wheel selection happens later in `plan.rs`/`prepare.rs` via `ocx_python` |
| `source/pypi.rs` | `source.type: pypi` discovery over the **Simple Repository API**: `GET {index}/{project}/` content-negotiated to PEP 691 JSON, falling back to a PEP 503 HTML anchor scan (Artifactory/Nexus serve only the latter). Versions come from the listed *filenames* (`uv_distribution_filename`, re-exported by `ocx_python`) — PEP 700's `versions` key is 1.1-only and has no HTML twin. One `VersionInfo` per version with ≥1 non-yanked file (PEP 592); PEP 440-aware prerelease flag; `assets` stays empty (wheel selection happens later, same as `pylock`). Indexes are tried in order and the first that **has** the project wins — never merged, which is the dependency-confusion guard |
| `auth.rs` | Host-keyed credentials for the mirror's own HTTP legs: `OCX_AUTH_<slug>_{TYPE,USER,TOKEN}` → `netrc` (`$NETRC`, else `~/.netrc`) → anonymous, resolved per **request URL** so one lock naming several hosts sends each host only its own credential. Nothing in `mirror.yml` names a variable — a contributed spec able to do so is a spec able to exfiltrate one (the reason npm/pnpm dropped env expansion from project-level `.npmrc`). The OCI legs keep `ocx_lib::auth`'s own ladder; netrc is deliberately absent there |
| `http.rs` | The only constructor for a mirror-owned `reqwest::Client`: bundled Mozilla roots **and** the platform trust store, so a distroless host still works and `SSL_CERT_FILE`/`SSL_CERT_DIR` reach a corporate CA. ocx solved this in its own reqwest major (`ocx_lib::utility::tls`); a path dep does not inherit it |
| `pipeline/orchestrator.rs` | `execute_mirror()`: prepare (concurrent) + push (sequential) |
| `pipeline/download.rs` | Single GET buffered to a file — no retry, no resume; an empty body is an error |
| `pipeline/lock_derive.rs` | `pipeline plan`'s per-candidate PEP 751 lock derivation for `source.type: pypi`: shells `uv pip compile` (indexes map onto `--index`/`--default-index` — uv treats the default as *lowest* priority, so the last entry is it — with `--index-strategy first-index` pinned, and credentials injected as `UV_INDEX_<NAME>_*` env, never argv) — universal locks (the default) via `--python-version X.Y`, no interpreter on disk; only `universal: false` materializes the pinned interpreter via `ocx package pull` (`--python <path>`) — relaxes the `requires-python` floor (uv#15995), stamps a provenance header, fail-closed re-parses via `ocx_python::parse_pylock` |
| `pipeline/python_prepare.rs` | pylock/pypi env-prepare path (parallel to the archive `orchestrator::prepare_version`): per (version, wheels key) download wheels → verify(sha256==lock) → repack → collide → `compose_env` → write `metadata.json` + N `tar.zst` layers + `env-manifest.json`; entry `platform` = full wheels key (push `-p` verbatim), `platform_slug` = base slug (JUnit naming) |
| `pipeline/python_push.rs` | pylock/pypi env-push helpers: read `env-manifest.json`, build the multi-layer `ocx package push --cascade --new -m META LAYERS…` invocation, spawn it; `register_wheel_layers` also pushes each not-yet-published wheel standalone to its content-addressed `pip-packages/...:<sha256>` repository first, so the app's own layer args' `:from=` mount tail has a source blob to reuse |
| `pipeline/ocx_cli.rs` | The `ocx` subprocess boundary: binary resolution and `OCX_*` env forwarding |
| `pipeline/ocx_cli/push.rs` | `ocx package push` — argv assembly, one attempt, the retry ladder (`PUSH_TIMEOUT`, `push_once`, `push_with_retry`) |
| `pipeline/ocx_cli/announce.rs` | `ocx package announce` — token, `TagSource`, argv, one bounded invocation |
| `pipeline/verify.rs` | Checksum verify |
| `pipeline/package.rs` | Extract archive, apply metadata, rebundle |
| `pipeline/push.rs` | Push to registry + cascade tag compute |
| `pipeline/mirror_task.rs` | `MirrorTask`: self-contained work unit |
| `pipeline/mirror_result.rs` | `MirrorResult`: Pushed/Skipped/Failed |
| `pipeline/registry_copy.rs` | The copy engine: by-digest transfer of manifests and blobs from a source registry into the destination registry (C-021…C-026, C-046) — classifies nothing, no tag parsed or cascade computed; a sibling of `pipeline/registry_sync.rs`, not a child, matching the crate's flat-siblings-with-child-directories shape (`orchestrator.rs`, `push.rs` + `push/alias.rs`) |
| `pipeline/registry_sync.rs` | `registry sync`'s run: pre-flight over every source (client → catalog → filter/expand), then collision detection across all sources, then the per-source copy pass, then the report (C-040, C-044); children own the rest |
| `pipeline/registry_sync/glob.rs` | `Glob` — compiled `include:`/`exclude:` package-name pattern, literal characters plus `*` only, built on the direct `regex` dependency (C-009, C-010) |
| `pipeline/registry_sync/destination.rs` | Where a copied package lands: `DestinationTemplate` expansion (`{registry}`/`{namespace}`/`{package}`), the OCI repository grammar guard, prefix containment, the `oci://` pointer, collision detection (C-011…C-015) — the destination trust boundary, every rule refuses rather than normalises |
| `pipeline/registry_sync/catalog.rs` | The source side: SSRF-guarded HTTP client + the three index-tree fetches (`config.json`, `c/index.json`, `p/<ns>/<pkg>.json`), plus the SSRF check on a root's physical `repository` host (C-016…C-020) — two separate trust mechanisms for two trust situations: the index base URL is operator-authored config, a root's `repository` pointer is foreign data |
| `pipeline/registry_sync/cache.rs` | The run's out-of-tree state: the source-catalog digest file and the lock directory, both derived from `sha256(canonicalized output path)` and kept outside `output:` (C-037, C-038) |
| `pipeline/registry_sync/index_write.rs` | Writing the servable index tree: store construction (locks redirected out of `output:`), the root rewrite + tag merge, the per-package `CatalogTransaction`, dispatch objects, `config.json`, the skip predicate, `--repair-catalog` (C-027…C-036, C-047) — writes are additive; the mirror owns its own tag-union merge because `CatalogTransaction::write_root` itself is merge-blind |
| `pipeline/registry_sync/plan.rs` | The pre-flight phase: filter → expand → collide → short-circuit → per-root fallback → work list, plus the `--dry-run` byte estimate (C-039, C-043) — runs before a single byte is copied, over every source |
| `pipeline/registry_sync/report.rs` | The run report + its two renderings (C-042, C-043's output half); local `OutputFormat` + free `report_*` function convention, since no `Printable` trait is reachable from this crate |
| `pipeline/dist_sync.rs` | `dist sync`'s run: fetch the upstream `dist.json`, apply `select:`, mirror each archive into `output:` at the rendered layout, rewrite each row's `url`, then write `dist.json` + `dist.json.sha256` + `dist/<sha256>.json` and optionally upload. Two invariants live here: **clobber-safety** (a run that cannot place every selected archive publishes no manifest at all — and its second half, a run that selected *nothing* publishes nothing either, since that case passes the partial-run guard trivially) and **publish order** (archives → snapshot → sidecar → rolling manifest last). The upstream manifest fetch is capped at 8 MiB the same way `registry_sync::catalog` caps an index document |
| `pipeline/dist_sync/manifest.rs` | The `dist.json` type, its filter/re-point pass, and the hand-rolled renderer. Unknown keys round-trip via `#[serde(flatten)]`; the rendering puts every leaf object on one line and emits `latest` first because `install.sh`'s `get_latest_version` / `dist_row` parse with `grep -o '{[^{}]*}'`, which is line-based. That cross-repo contract is pinned from this side by an acceptance test running the same pipeline — `ocx-sh/www-setup` shares no CI with this repository |
| `pipeline/dist_sync/layout.rs` | `LayoutTemplate` — plain substitution over `{version}`/`{tag}`/`{target}`/`{filename}`/`{channel}`, same closed-set doctrine as `DestinationTemplate`. The run's containment boundary: every substituted value comes off a foreign manifest and is refused, never normalised, if it is not a single safe path component |
| `pipeline/dist_sync/upload.rs` | The native HTTP PUT: one implementation (Artifactory generic, Nexus raw, GitLab generic, WebDAV are the same request; Azure differs by a header), HEAD-before-PUT idempotency, env-resolved credentials with redacting `Debug`, retry on transport/5xx/429 only — never 4xx — and `Retry-After` clamped to 300 s |
| `pipeline/dist_sync/report.rs` | The `dist sync` report + its two renderings; same local `OutputFormat` + free `report_*` convention as `registry_sync/report.rs` |
| `pipeline.rs` | Shared pipeline helpers (e.g. `propagate_exit_code`) |
| `annotations.rs` | GHA annotation emission for test failures |
| `discord.rs` | Discord webhook HTTP client |
| `junit.rs` | JUnit XML parser; produces `TestResult` per `(V, P, C, name)` |
| `run_summary.rs` | `RunSummary` schema (serialized to run-summary.json) |
| `version_platform_map.rs` | Tracks `(version, platform)` pairs across push legs |
| `normalizer.rs` | `normalize_version()`: add build timestamp |
| `resolver.rs` | `resolve_assets()`: apply regex patterns to asset names |
| `filter.rs` | `filter_versions()`: apply bounds, prerelease skip, backfill cap. Also `pep440_sort_key()` — the total order over version strings that `plan` and `push` sort by — and `version_cmp()`/`within_bounds()`, the min-inclusive/max-exclusive comparator every bound routes through (`versions.min`/`max`, `select_pypi_candidates`, per-platform windows, `exclude:` ranges): `ocx_lib::Version` first, PEP 440 for the ≥4-component releases it rejects, fail-open when neither parses |
| `error.rs` | `MirrorError` variants and exit code mappings |
| `lib.rs` | Library root. Public surface is `Command`, `error`, `spec` and nothing else — a wide surface would silence `dead_code`, which the crate denies on |
| `main.rs` | `Cli` + `main()`; everything else lives in the library |
| `test_support.rs` | `OCX_ENV_LOCK` — the crate-wide guard serialising tests that read or write the process-global `OCX_*` environment |

## Pipeline Architecture

**Two-phase**: prepare (concurrent) then push (sequential by version).

### Phase 1: Prepare (concurrent)

1. Fetch upstream versions (GitHub API or URL index)
2. Resolve assets per platform (regex match)
3. Filter versions (min/max, prerelease, backfill cap)
4. Parallel: download → verify → **extract → `bin_scan` → chmod declared binaries → libc check → write sidecar** → compress → drop tree (two independent semaphores: I/O vs CPU)

The bolded window is load-bearing. Every step in it must sit between extraction and compression, because the tree is gone afterwards: `bin_scan` derives the `binaries` claim from it, the chmod makes the files that claim names executable, and `libc_lint` reads each interface binary's `PT_INTERP` to check the declared `os.features`. None is recomputable on a resume, and they are not equally covered afterwards: `bin_scan`'s resume path re-runs its own guard (`reject_empty_scan`), the libc check has no equivalent and is reachable only from the bundle block, after the `bundle_path.exists()` early return. A bundle on disk is therefore **not** evidence the libc check passed — it may have been written under `libc_lint: false`, or by a binary predating the check — and flipping `libc_lint` back to `true` does not reach a work dir that already holds a bundle; the operator must discard it. No warning fires for this: it would fire on every resumed run with the on-by-default value. The declared-binaries chmod is uncovered the same way: a bundle written by a binary predating it keeps its 0644 members, and no resume will fix them — discard the bundle to re-prepare it. The sidecar is written last so a refused version leaves nothing publishable behind.

### Phase 2: Push (sequential by version, oldest first)

1. Push bundle to registry, each attempt bounded by a 3600s timeout; a
   transient failure (`ocx package push` exit 75 only — exit 69 is never
   retried, since a rerun would not change that outcome) is retried up to
   `concurrency.max_retries` extra attempts with 1s-doubling-to-30s-capped
   backoff plus ±10% jitter
2. Cascade derived tags if enabled (X.Y.Z → X.Y → X → latest)
3. Track pushed (version, platform) pairs for cascade correctness
4. Complete the cascade for the platforms an EARLIER run published
   (`push.rs::cascade_backfilled_entries`)

Step 4 exists because `--cascade` merges only the pushed leg's own platform
entry into each rolling tag, so both push loops give it to *every* leg of a
whole version. That covers a version published in one run and not one completed
across two: the first run withholds `--cascade` while the version is still
partial, and `pipeline plan` trims the tiles it published from the backfill
run, so nothing ever cascades them and `X.Y`/`X`/`latest` end up holding the
backfilled platform alone. The repair re-emits each such entry from the
registry's own descriptors (published layers by digest + published config
metadata verbatim, the `pipeline patch` mechanism), and **skips** any entry
whose config bytes the running build would not reproduce exactly — a re-push
there would rewrite the platform manifest digest instead of only moving tags.
Best-effort: the packages are published either way, so a failure warns.

The 75/69 split above only exists from **ocx ≥ 0.5.3** onward — the `ocx`
binary that actually runs the push subprocess, i.e. whatever `ocx.toml` /
`ocx.lock` toolchain the running `ocx-mirror` is co-located with, not the
separately-pinned `ocx` version a *generated* downstream workflow bakes into
its own `setup-ocx` step. An older `ocx` maps every registry-client failure to
exit 69, so a stale toolchain pin retries nothing regardless of
`concurrency.max_retries`, and there is deliberately no runtime warning for
it — the invariant is enforced by keeping this repository's own `ocx.toml` /
`ocx.lock` current, the same discipline the prepare-window invariant above
depends on.

**ocx ≥ 0.5.5 is a hard floor, not a degradation.** From 0.5.5 the sidecar
`pipeline prepare` writes carries no top-level `platform` key (the platform
travels on `--platform`), and an older `ocx package push` / `package test`
demands the key and exits **65** — not a retried code — on every push leg.
`ocx.toml` pins `ocx` itself for exactly this reason, so a local `task verify`
and CI agree; `.github/workflows/verify.yml`'s `setup-ocx` step carries the
same floor and moves with the submodule pointer.

## Spec Format (YAML)

Key fields: `name`, `target` (registry + repo), `source` (GithubRelease or UrlIndex), `assets` (platform → regex[]; keys may carry a `+libc.glibc`/`+libc.musl` suffix to publish per-libc variants sharing one os/arch), `asset_type` (Archive/Binary), `cascade` (bool, or a map `{schedule}` that also puts the generated repair workflow on a timer), `versions` (min/max/new_per_run/backfill), `verify`, `concurrency`, `bin_scan` (off/auto/verify), `libc_lint` (bool, default **on** — total opt-out for the create-time libc check, mirroring `ocx package create --no-libc-lint`). Env sources (`pylock`/`pypi`) replace `assets`/`asset_type`/`variants` with the required `wheels:` map (`spec/wheels.rs`); libc is an `os.features` platform axis there — no variant/libc tag prefix, one image index, per-key entries (`build_timestamp` stamps an env tag exactly as it stamps an archive one); push gates each entry's JUnit by libc-compatible container legs (`spec::infer_libc_from_image`, shared from `src/spec.rs`).

Source types:
- `github_release`: `{owner, repo, tag_pattern}` — regex with `(?P<version>...)` capture
- `url_index`: Remote URL, inline versions, or generator command

Spec inheritance via `extends:` (shallow merge, child override parent).

### Per-platform version applicability

`platforms.<p>` carries `min_version` (inclusive) / `max_version` (exclusive) / `exclude` (list of `ExcludeEntry`: single `version` or a `min_version`/`max_version` range, `severity: broken|skip` default `broken`, optional `reason`). The single source of truth is two predicates on `MirrorSpec`: `platform_applies(version, platform)` and `exclude_hit(version, platform)` (both strip build metadata via `parent()` before comparison, reusing the `filter.rs` min-inclusive/max-exclusive convention).

Enforcement choke points:
- **Resolve** — `plan.rs::build_plan_report` + `prepare.rs::build_tasks_for_version` drop non-applicable `(V,P)` via `platform_applies`, so they never reach `plan.json`, are never scheduled/built/tested, and never red the run.
- **Test matrix** — the generated `workflow.yml` test loop skips a version when `matrix.platform ∉ version.platforms` (the discover output already excludes them). Same mechanism fixes the backfill-partial false-red.
- **Push visibility** — `push.rs::collect_excluded_platforms` records `severity: broken` excludes into `VersionSummary.platforms_excluded` (`ExcludedPlatform { platform, reason }`); `skip` stays silent.

To re-enable a pair, delete the entry (next clean run backfills). Use these fields instead of bumping the global `versions.min` for a late-added / dropped / broken platform.

### Discord notify

`notify.discord.user_id` (snowflake, non-secret) is inlined by the renderer into the notify job env as `OCX_MIRROR_DISCORD_USER_ID`. `notify.rs` emits **one Discord message per published version** (one embed each — avoids Discord's 1024-char field cap and reads as a distinct notification per release); a message carrying a partial/failed version is prefixed with a scoped `<@id>` mention. Consecutive POSTs are paced (~750ms) and `discord.rs::post` retries HTTP 429 honoring `retry_after` (3 retries, capped) to stay under Discord's webhook rate limit. `discord.rs` carries `content` + `allowed_mentions` (`parse: []` + explicit `users` so only that user pings). 🔒 rows render `platforms_excluded`.

## Error Model

`MirrorError` enum with exit codes. See `src/error.rs::MirrorError::kind_exit_code`.

| Variant | Exit code | Meaning |
|---------|-----------|---------|
| `SpecInvalid` | 65 (DataError) | Schema validation failed |
| `SpecNotFound` | 79 (NotFound) | `mirror.yml` not found at spec path |
| `ExecutionFailed` | 1 (Failure) | Mirror pipeline execution error |
| `SourceError` | 69 (Unavailable) | Upstream source unreachable |
| `PylockError` | 65 (DataError) | `source.type: pylock`/`pypi` resolution failure: no locked package matches the app name, invalid platform/variant, no compatible wheel (`select_wheels`), wheel sha256 ≠ lock hash, collision, or compose failure — all malformed spec/lock content, not a transient resource. Also covers a `pypi` lock-derivation `uv pip compile` non-zero exit or fail-closed re-parse rejection (`lock_derive.rs`). `RepackError` maps to `ExecutionFailed` (1); download failure to `SourceError` (69); `uv` binary missing/spawn failure, timeout, or interpreter materialization failure also maps to `ExecutionFailed` (1) |
| `PypiError` | 65 (DataError) | `source.type: pypi` discovery failure classified as malformed input: a Simple API 404 from **every** configured index (the package name exists on none of them). A genuinely unreachable index — connection refused, timeout, 5xx, 401, malformed body — stays `SourceError` (69), because it means "unknown", not "absent"; see `source::pypi::classify_error` |
| `TargetError` | 69 (Unavailable) | Target registry read failed (tag list / manifest fetch) — fail-safe abort instead of re-flagging published versions as new (issue #157) |
| `SpecUsageError` | 64 (UsageError) | Invalid `mirror.yml` usage: hardcoded webhook URL, empty `tests:`, ambiguous shell, `ocx_install:` block |
| `RendererDrift` | 65 (DataError) | `--check` mode: generated files differ from current spec |
| `JunitParseError` | 65 (DataError) | JUnit XML parse failure in `pipeline push` |
| `RunSummaryError` | 65 (DataError) | Cannot read or write `run-summary.json` |
| `PlanError` | 65 (DataError) | `plan.json` missing/malformed, version absent from plan, or plan lacks resolved assets (`prepare --plan`) |
| `TemplateError` | 74 (IoError) | Workflow template render failure |
| `WebhookUnavailable` | 69 (Unavailable) | Discord 5xx / timeout in `pipeline notify` |
| `WebhookPermissionDenied` | 77 (PermissionDenied) | Discord 401/403 — webhook secret likely rotated |
| `CascadeUnrepaired` | 65 (DataError) | `ocx package cascade repair` ran and findings remain — carries `ocx`'s own exit 65 through unchanged, so an audit result never reads as the tool breaking (which stays `ExecutionFailed`) |
| `IndexWriteError` | 74 (IoError) | A local filesystem write into the served index tree under `output:` failed (root document, `c/index.json`, `config.json`, dispatch object, `--repair-catalog`) — always aborts the run, never the package. **Two classes, not one**: `registry_sync/index_write.rs`'s own `refused()` helper documents the split by *whose fault it is* — a shape or validation refusal of *upstream* bytes (a root that is not a JSON object, a dispatch object that fails `validate_image_index`) fails only that package as `ExecutionFailed` (1) instead, so a hostile source cannot deny a whole mirror by publishing one malformed package; only a genuinely local write failure (disk full, `EACCES`, a broken output tree) is `IndexWriteError` |
| `IndexFormatUnsupported` | 65 (DataError) | A source index declared a `config.json` `format_version` above the one `ocx_lib` supports (carries the declared version) — not transient, so `SourceError` (69) would make CI retry forever on something retry can never fix; the run writes nothing |

## Test Pipeline {#test-pipeline}

`ocx-mirror package pipeline` is a family of six subcommands that together implement per-mirror CI pipelines. The pipeline smoke-tests every `(version, platform)` pair before publishing to the registry, preventing broken packages from reaching users.

### Subcommand contracts

| Subcommand | Role in pipeline | Key invariant |
|-----------|-----------------|---------------|
| `pipeline generate ci` | Renderer — writes `.github/workflows/{mirror,describe,verify-generated}.yml` | Idempotent; `--check` exits 65 on drift. Emits `verify-generated.yml` (drift guard, R4) unless `allow_manual_edits: true`. Rejects hardcoded webhook URLs at parse time (R3) |
| `pipeline plan` | Discover — find new work | Side-effect-free for `github_release`/`url_index`/`pylock`; calls registry + source, emits `plan.json` (schema v2 carries resolved asset URLs per entry). For `source.type: pypi`, additionally derives one PEP 751 lock per candidate version into `--locks-dir` (default `./locks`, via `lock_derive.rs`) — not side-effect-free for that source type; each entry's `pylock` field carries the derived lock's path |
| `pipeline prepare --version V [--plan plan.json]` | Prepare — download + bundle | One version across all platforms; writes `bundle-{V}-{P}.tar.xz` per platform. With `--plan`, tasks come from the plan's resolved assets — the source is never queried (one crawl per run, issue #160); without it, falls back to a standalone crawl |
| `pipeline push` | Push — publish greens | Serial driver; AND across containers for each `(V, P)`; sole cascade-tag writer in the publish pipeline (`pipeline cascade` below re-points existing aliases on dispatch) |
| `pipeline notify` | Notify — Discord report | Reads `run-summary.json`; silent when all skipped-existing |
| `pipeline cascade [--dry-run]` | Repair — re-point broken rolling aliases (dispatch; timer when `cascade.schedule` is set) | Drives `ocx package cascade repair`; exit 65 = findings remain, everything else non-zero = exit 1. Announces the tags it moved even after a 65, never on a dry run |

### R1: Cross-mirror concurrency invariant

Generated workflows include a workflow-level `concurrency:` block:

```yaml
concurrency:
  group: mirror-${{ github.workflow }}-publish
  cancel-in-progress: false
```

`cancel-in-progress: false` ensures a push job is never aborted mid-flight, preventing cascade-tag corruption. Different mirror repos use different workflow names so the group key remains repo-scoped.

`cascade.yml` joins the **same** group rather than owning one: a repair and a live push both re-point the same rolling aliases. It cannot read the push workflow's `github.workflow`, so the renderer bakes the resolved literal (`mirror-<spec.name>-publish`, since the push workflow's `name:` *is* `spec.name`) into the generated file — `publish_concurrency_group` in `ci.rs`, pinned by a test that derives both ends from one render.

The guarantee is mutual exclusion, not a queue: GitHub holds one *pending* run per group, so the run waiting behind the live one is cancelled when a newer run of either workflow arrives. A scheduled repair can therefore be dropped by a busy publish (grey "cancelled", never red), and a pending publish can be dropped by a repair — accepted, since Actions offers no mutex primitive and neither outcome corrupts tags. A spec must not give `cascade.schedule` the same cron as `versions.poll_interval`.

`announce-from-registry.yml` keeps a group of its own (`mirror-${{ github.workflow }}-announce-from-registry`) and must **not** be folded into the publish group, even though it too can carry a schedule (`announce.schedule`). It writes index pull requests only, never registry tags, so concurrent announce writers contend on a per-package index branch: the fast-forward path is CAS with a retry, and the spent-branch reset path can drop a racing branch commit, which the next full from-registry run re-adds. Joining the publish group would instead let a queued push cancel the pending catch-up. The publish-group coupling exists solely because cascade and push re-point the same rolling aliases. A spec must not give `announce.schedule` the same cron as `versions.poll_interval` either — that schedules the catch-up against the push job's own closing announce.

### R3: Webhook URL rejection invariant

`policy_check_notify` in `spec/validate.rs` validates the `discord.webhook_secret` field at spec parse time. Any value matching `discord.com`, `discordapp.com`, or the pattern `^https?://` is rejected with `SpecUsageError` (exit 64) before any file is written. The webhook URL never appears in generated files or in log output.

### R4: Generated drift guard (`verify-generated.yml`)

`pipeline generate ci` emits `.github/workflows/verify-generated.yml` alongside each spec's `mirror.yml` + `describe.yml` + `patch.yml` (+ `cascade.yml` when the spec cascades, + `announce-from-registry.yml` when the spec announces). On `pull_request` + push to `main` it runs `ocx-mirror package pipeline generate ci --check` (called directly — setup-ocx activates the project toolchain onto PATH), which re-renders from `mirror.yml` and exits 65 on drift — so a hand-edit to any generated workflow fails CI (forbids manual edits to the generated surface). The guard checks all rendered files, including itself.

**Pins are mirror-repo-owned.** Before diffing, `check_drift` normalizes every `uses: owner/action@<ref>` line (`normalize_for_drift` in `ci.rs`), stripping the `@<ref>` digest/tag and any trailing `# vX` comment. A downstream Renovate/Dependabot bump of an action pin therefore does **not** trip the guard — the mirror repo owns its pins. The `owner/action` identity is preserved, so swapping in a *different* action, or any change to step logic, still reds. Templates ship a known-good seed pin (incl. `ocx-sh/setup-ocx`, SHA-pinned) for the first render.

Opt-out (discouraged): top-level `allow_manual_edits: true` in `mirror.yml`. When set, the renderer omits `verify-generated.yml` and `execute()` prints a stderr note so the disabled guard is never silent. Use only when a repo deliberately maintains its workflows by hand. Field lives on `MirrorSpec` (`spec.rs`), defaults `false`.

### Cross-references

- Design spec: `.claude/artifacts/system_design_mirror_test_pipeline.md` — component contracts, CLI shape, GHA job contracts, install strategy
- ADR: `.claude/artifacts/adr_ocx_mirror_test_pipeline.md` — rationale, risk register, open-call resolutions

## Test Layout

Unit tests do not live inline in the module they exercise. Each module's test
corpus sits in a sibling `tests/` directory, wired with
`#[cfg(test)] #[path = "<mod>/tests.rs"] mod tests;` — a `#[path]` module is
still a child, so `use super::super::*;` reaches private items unchanged.
Cross-topic helpers go in that directory's `support.rs`.

`#[path]` resolves a module's children against the directory holding its own
file rather than a subdirectory named after it, so each child names its own
path too. That is what keeps the corpus in `<mod>/tests/` and leaves `<mod>/`
for production modules.

Two integration surfaces sit outside the crate: `tests/spec_validation.rs`
drives `tests/fixtures/invalid/*.yml` (one fixture per rejected document,
expectations declared as leading `# expect:` comments), and `tests/golden/`
holds the rendered-workflow corpus. The library target exists to make both
possible; its public surface is deliberately `Command`, `error` and `spec` and
nothing else, so `dead_code` keeps working on everything else.

## Quality Gate

During review-fix loops, run `task rust:verify` — not full `task verify`.
Full `task verify` = final gate before commit.