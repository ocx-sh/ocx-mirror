# mirror.yml Reference

`mirror.yml` describes one tool to mirror — where to fetch upstream releases, which platforms to build for, how to test each bundle, and how to report results. The file is consumed by `ocx-mirror package sync`, `ocx-mirror package check`, and all `ocx-mirror package pipeline` subcommands.

## Top-level keys {#top-level}

| Key | Type | Required | Purpose |
|-----|------|----------|---------|
| `name` | string | Yes | Tool name, used in log output and notify messages |
| `target` | object | Yes | OCI registry and repository to push to |
| `source` | object | Yes | Upstream release source ([GitHub Releases][github-releases] or URL index) |
| `assets` | object | Yes* | Platform → regex list mapping for selecting upstream release archives. *Mutually exclusive with `variants` — exactly one of the two is required. |
| `variants` | array | No* | Alternate asset sets for the same tool, each producing its own version-tag prefix. *Mutually exclusive with `assets` — exactly one of the two is required. See [`variants`](#variants). |
| `metadata` | object | No | Path(s) to the package metadata JSON, with optional per-platform overrides. See [`metadata`](#metadata). |
| `asset_type` | string | No | `Archive` (default) or `Binary` |
| `build_timestamp` | string | No | Per-build tag suffix: `datetime` (default), `date`, or `none`. See [build_timestamp & GC-safe publishing](#build-timestamp). |
| `cascade` | boolean | No | Cascade rolling tags on push (`true` by default). See [build_timestamp & GC-safe publishing](#build-timestamp). |
| `versions` | object | No | Version filter (min/max bounds, `new_per_run`, backfill order) |
| `verify` | object | No | Checksum verification options |
| `concurrency` | object | No | Parallel download and push limits |
| `tests` | array | No* | Commands to run against each installed bundle. Required when `pipeline generate ci` is used. |
| `platforms` | object | No* | GHA runner and container matrix. Required when `pipeline generate ci` is used. |
| `ocx_mirror` | object | No | Provenance of the ocx-mirror behind a plan. Pins nothing. |
| `notify` | object | No | Discord webhook notification settings |
| `announce` | object | No | Index announce settings. See [`announce`](#announce). |
| `annotations` | object | No | Extra OCI annotations written onto every published image index. See [`annotations`](#annotations). |
| `catalog` | object | No | README + logo published as registry catalog metadata. See [`catalog`](#catalog). |

The `tests`, `platforms`, `ocx_mirror`, `notify`, `announce`, and `catalog` keys are used only by `ocx-mirror package pipeline` subcommands. `sync` and `check` ignore them.

## `target` {#target}

The registry and repository a push writes to — the **physical** path.

```yaml
target:
  registry: ghcr.io
  repository: ocx-contrib/bazelbuild/bazelisk
```

Path segments are separated by `/` and are never flattened into a hyphen: `ocx-contrib/bazelbuild/bazelisk`, not `ocx-contrib/bazelbuild-bazelisk`. GHCR needs no repository of its own for a path segment — package-to-repository linkage comes from the [`org.opencontainers.image.source`](#annotations) annotation, which the pipeline writes automatically.

The physical path and the [logical index package](#announce) are related by convention, not by a rule: a mirror publishing to `ghcr.io/ocx-contrib/bazelbuild/bazelisk` announces the logical package `bazelbuild/bazelisk`. Spell both out.

When `registry` is `ghcr.io`, generated workflows log in with the run's own `GITHUB_TOKEN` and `github.actor`, and the push job declares an explicit `permissions:` block. The shared `OCX_MIRROR_REGISTRY_USER` / `OCX_MIRROR_REGISTRY_TOKEN` organisation secrets carry `ocx.sh` credentials and are not read on that path.

**The first path segment must be the mirror repository's own owner.** `GITHUB_TOKEN` authorises packages owned by the repository it runs in; `docker login ghcr.io` succeeds regardless — logging in is not authorisation — and the push then fails with `denied: installation not allowed to Create organization package`. So a mirror in `ocx-contrib/mirror-bazelisk` can publish `ghcr.io/ocx-contrib/…` without any secret being configured, and cannot publish under another owner without one. `generate ci` warns when it can see the mismatch (it reads `GITHUB_REPOSITORY`, so on a runner always, and locally only when that variable is set).

Declaring any permission sets every unnamed scope to `none`, so the generated block names every scope the push job's steps need:

```yaml
    permissions:
      contents: read          # checkout, setup-ocx
      packages: write         # docker login + ocx package push
      actions: read           # resolving the job URL for the notification links
      checks: write           # test-result check run
      pull-requests: write    # test-result pull-request comment
```

## `assets` {#assets}

Maps a **platform key** to an ordered list of regexes. Each regex is matched against upstream asset filenames; the first platform with exactly one distinct match resolves to that asset (zero matches = platform absent for that version, two or more = ambiguous error).

A platform key is `<os>/<arch>` with optional suffixes:

```
<os>/<arch>[/<variant>][/<os_version>][+libc.<flavor>...]
```

```yaml
assets:
  linux/amd64:
    - "tool-.*-linux-x86_64\\.tar\\.gz"
  darwin/arm64:
    - "tool-.*-darwin-arm64\\.tar\\.gz"
```

### libc variants {#assets-libc}

When a tool ships separate builds for different C libraries on the same `os/arch` (e.g. glibc and musl on `linux/amd64`), append a `+libc.<flavor>` tag to the key. The tag is published into the OCI image index as an `os.features` entry, so a client (`ocx add`) selects the build matching its host libc:

```yaml
assets:
  "linux/amd64+libc.glibc":
    - "cpython-.*-x86_64-unknown-linux-gnu.*\\.tar\\.zst"
  "linux/amd64+libc.musl":
    - "cpython-.*-x86_64-unknown-linux-musl.*\\.tar\\.zst"
```

`libc.glibc` and `libc.musl` are the recognized flavors. The two keys are distinct platforms — each needs its own regex list, and each publishes as its own image-index entry. A key with no `+libc.` tag carries no libc requirement and resolves for any host (the pre-libc behavior). Quote keys containing `+` so YAML parses them as strings.

## `variants` {#variants}

Some tools publish more than one build flavor from the same release — a PGO/LTO-optimized build alongside a regular one, a `slim` image alongside the full one. `variants:` replaces top-level `assets:` with a named list of asset sets, each becoming its own version-tag prefix (`slim-3.13.9` vs. the bare `3.13.9`), so the flavors share one `mirror.yml` and one generated pipeline instead of splitting into a spec per flavor.

```yaml
variants:
  - default: true
    assets:
      linux/amd64: ["cpython-.*-x86_64-unknown-linux-gnu\\.tar\\.zst"]
  - name: pgo.lto
    assets:
      linux/amd64: ["cpython-.*-x86_64-unknown-linux-gnu-pgo\\+lto\\.tar\\.zst"]
    metadata:
      default: metadata-pgo-lto.json
```

**Fields (per entry):**

| Field | Type | Required | Description |
|-------|------|----------|--------------|
| `name` | string | Yes, unless `default: true` | Tag prefix for this variant's versions. Must match `^[a-z][a-z0-9.]*$`; `latest` is reserved and rejected — it would collide with the cascade alias the default variant already produces. |
| `default` | boolean | No | Marks the variant whose tags publish unprefixed (`3.13.9`, not `<name>-3.13.9`). Exactly one variant must set this. |
| `assets` | object | Yes | This variant's platform → regex mapping, same shape as top-level [`assets`](#assets). |
| `metadata` | object | No | Overrides the top-level [`metadata`](#metadata) for this variant only. |
| `asset_type` | string | No | Overrides the top-level `asset_type` for this variant only. |

**Rules:**

- Mutually exclusive with top-level `assets:` — a spec sets exactly one of the two.
- At least one variant is required; exactly one must set `default: true`.
- Only the default variant may omit `name`. A non-default variant without one is rejected.
- Two variants with the same name (or two unnamed variants) are rejected as duplicates.
- A variant that omits `metadata` or `asset_type` inherits the spec's top-level value.

## `metadata` {#metadata}

Points at the package metadata JSON that `ocx package test` and `ocx add` use to install a mirrored bundle — the `env`/`type` document, not anything CI-specific.

```yaml
metadata:
  default: metadata.json
  platforms:
    darwin/amd64: metadata-darwin.json
    darwin/arm64: metadata-darwin.json
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|--------------|
| `default` | string | Yes, when `metadata:` is present | Path to the metadata file, relative to the spec's own directory. |
| `platforms` | object | No | Platform key → metadata path, relative to the spec's own directory. A listed platform uses this file instead of `default` — for a tool whose install layout differs by platform (e.g. a macOS `.app` bundle needs a different `PATH` entry than the Linux layout). |

Both paths resolve against the directory holding the spec file, never the repository root — the same rule [`catalog`](#catalog) follows, and the opposite of [`tests.script`](#multi-spec-script-path). Every path is checked for existence when the spec loads; a missing file is a spec-load failure (exit 65), not a runtime surprise.

Editing this file only changes what *future* pushes publish. `pipeline plan` compares it against what already-published versions record and reports any mismatch as a `metadata-drift` entry; [`pipeline patch`][cli-patch] then republishes the corrected metadata against those versions' existing layers, with no re-download and no re-upload. There is no key here for *which* versions to patch — that range is a flag on the `patch` command line, not spec state, since a stored range would need a ledger to track what it had already covered.

## `build_timestamp` & GC-safe publishing {#build-timestamp}

`build_timestamp` controls the tag a mirrored version is published under. Each `(version, platform)` push writes a **primary tag** for that version; with `cascade: true` (the default) it also re-points the **rolling tags** `X.Y`, `X`, and `latest` to the newest build.

| Value | Primary tag for `3.28.0` | Effect |
|-------|--------------------------|--------|
| `datetime` (default) | `3.28.0_20260310142359` | Unique per build (UTC `YYYYMMDDHHMMSS`). Never re-pointed. |
| `date` | `3.28.0_20260310` | Unique per build-day (UTC `YYYYMMDD`). |
| `none` | `3.28.0` | Bare version tag. Re-published in place on every rebuild. |

Pre-releases keep their identifier: `3.28.0-rc1` → `3.28.0-rc1_20260310142359`. A version that already carries a build suffix is rejected rather than double-stamped.

!!! warning "The garbage-collection hazard of `build_timestamp: none`"
    A digest is immutable, but a *tag* is not. Re-publishing a version under `build_timestamp: none` — or moving a rolling cascade tag to a newer build — re-points the tag and leaves the previous digest **untagged**. Once untagged, registry garbage collection can reap it, breaking any consumer `ocx.lock` pinned to that `@sha256:` digest. "Digests are immutable" only holds until GC runs.

    With `datetime` or `date`, every build also lands under its own unique `X.Y.Z_<ts>` tag that is never re-pointed, so the digest stays permanently reachable even as the rolling cascade tags float. This is the **GC-safe** choice. Trade-off: storage grows with every build, and the version tag is no longer bare.

**Choosing a value:**

- **`datetime` (default)** — GC-safe, no registry configuration required. Recommended for any mirror whose packages are pinned by digest downstream.
- **`date`** — GC-safe across days with coarser tags. Caveat: a second build on the same UTC day re-points that day's tag, orphaning the earlier same-day digest — the within-day hazard remains.
- **`none`** — bare tags only. Use exclusively when the target registry protects referenced digests from GC: a retention policy that keeps untagged manifests still referenced by consumers, an OCI referrers/lock guard, or a guarantee that a version is never re-published (each `X.Y.Z` treated as immutable upstream).

`ocx-mirror` emits a parse-time warning when `build_timestamp: none` is combined with `cascade`, so the hazard surfaces on every `validate`, `check`, `sync`, and `pipeline` run. It is advisory, not fatal — a registry with retention configured can use `none` safely.

## `tests` {#tests}

Declares the smoke-test commands to run against each installed bundle. Every entry runs for every `(version, platform, container)` combination in the matrix.

```yaml
tests:
  - name: version
    command: cmake --version
  - name: smoke
    script: tests/smoke.star
```

Each entry sets exactly one of three mutually exclusive fields:

| Field | Type | Description |
|-------|------|-------------|
| `command` | string | Single-line shell command, executed verbatim in the leg's configured shell. Multi-line logic must live in a repository file invoked via shell (`bash ./tests/smoke.sh`, `pwsh -File ./tests/smoke.ps1`). |
| `script` | string | Path to a [Starlark][starlark] `.star` file, run via `ocx package test --script`. **Resolves from the repository root**, not from the spec's own directory — see [`script:` resolves from the repository root](#multi-spec-script-path) for where that matters in a multi-spec repository. |
| `script_inline` | string | Starlark source given inline (YAML `\|` block scalar), piped to `ocx package test --script -`. |

**Rules:**

- Required: must contain at least one entry when used with `pipeline generate ci`.
- `name` must be unique within the file and must match `^[a-zA-Z][a-zA-Z0-9_-]*$`. The name appears as the JUnit test-case name, so it must be stable across runs.
- Exactly one of `command`, `script`, `script_inline` per entry — zero set or more than one set is rejected at validation time.

**Environment exposed to every test command:**

| Variable | Value |
|----------|-------|
| `OCX_INSTALL_DIR` | Path where `ocx package test` materialized the package |
| `OCX_VERSION` | Mirrored version string (e.g., `3.29.0`) |
| `OCX_PLATFORM` | Platform slug (e.g., `linux/amd64`) |
| `OCX_IMAGE` | Container image; empty on native legs |
| `OCX_TEST_NAME` | The `tests[].name` value for this invocation |

## `platforms` {#platforms}

Declares the GHA runner and container matrix for the generated workflow. Each key is a platform key, in the same form [`assets`](#assets) uses — including the `+libc.<flavor>` suffix.

A platform without `containers:` runs its tests natively on the runner. A platform with `containers:` runs them once per image: the generated workflow fetches a libc-matched, statically-linked `ocx` release and executes every `ocx package test` inside `docker run <image>`, so the mirrored artifact is loaded and run by that image's own libc. That is the only way an `os.features` musl or glibc claim is actually verified — an artifact that links glibc reds its Alpine leg instead of shipping a false claim.

!!! note "Container legs are linux-only, and run natively"
    Tests run via `docker run`, so `containers:` is rejected on a `darwin/*` or `windows/*` platform. A spec may mix freely: container legs on Linux, native legs everywhere else. No qemu is installed, so a `linux/arm64` platform with `containers:` needs an arm64 `runner:` — the leg fails with that message rather than emulating.

Making a libc claim provable is the point of the container matrix, so the platform key carries it:

```yaml
platforms:
  "linux/amd64+libc.musl":
    runner: ubuntu-latest
    containers:
      - { image: "alpine:3.20", shell: sh }

  "linux/amd64+libc.glibc":
    runner: ubuntu-latest
    containers:
      - { image: "ubuntu:24.04", shell: bash }
```

`docker run --platform` is handed the key with the `+libc.*` suffix stripped (`linux/amd64`); `ocx package test --platform` keeps the full key, which is what selects that entry out of the image index.

```yaml
platforms:
  linux/amd64:
    runner: ubuntu-latest
    containers:
      - { image: "ubuntu:24.04", shell: bash }
      - { image: "alpine:3.20",  shell: sh }
      - { image: "fedora:40",    shell: bash }

  linux/arm64:
    runner: ubuntu-24.04-arm
    containers:
      - { image: "ubuntu:24.04", shell: bash }
      - { image: "alpine:3.20",  shell: sh }

  darwin/arm64:
    runner: macos-latest

  darwin/amd64:
    runner: macos-latest
    prefix: ["arch", "-x86_64"]

  windows/amd64:
    runner: windows-latest
    shell: pwsh
    tests:
      - name: version
        command: cmake.exe --version
      - name: smoke
        command: pwsh -File ./tests/smoke.ps1
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `runner` | string | Yes | [GitHub Actions][github-actions-docs] runner label |
| `containers` | array | No | Container matrix entries. Absent = native mode. Must have ≥1 entry when present. |
| `containers[].image` | string | Yes | Valid OCI image reference (e.g. `ubuntu:24.04`) |
| `containers[].shell` | string | No* | Shell to invoke inside the container. *Required when image name does not match a known default (see below). |
| `shell` | string | No | Default shell for native legs. Defaults: `pwsh` on Windows, `bash` elsewhere. |
| `prefix` | array of strings | No | Command prefix applied before every test invocation. Defaults: `["arch", "-x86_64"]` on `darwin/amd64` with a `macos-*` runner; empty otherwise. |
| `tests` | array | No | Per-platform test override. When present, replaces the top-level `tests:` array entirely (no partial merge). |
| `min_version` | string | No | Inclusive lower bound: the first upstream version this platform applies to. See [Version applicability](#platform-version-applicability). |
| `max_version` | string | No | Exclusive upper bound: the first upstream version this platform no longer applies to. |
| `exclude` | array | No | Individual `(version[, range])` holes within the window. See [Version applicability](#platform-version-applicability). |

**Platform key validation:**

- Must parse as a platform key: `<os>/<arch>[/<variant>][+libc.<flavor>,...]` — the same grammar [`assets`](#assets-libc) uses. Quote any key containing `+`.
- A key declaring a libc must be tested under that libc: every image on `linux/amd64+libc.musl` has to be a musl base (Alpine), and every image on a `+libc.glibc` key a glibc base. The mismatch is rejected at generate time with exit 65 — a musl-static binary runs fine under glibc, so an Alpine claim tested in Ubuntu goes green having verified nothing.

### Version applicability {#platform-version-applicability}

Not every platform applies to every release. A platform may be **introduced late** upstream (its first binary ships at some `0.11.7`), **dropped** at a later release (the upstream stops shipping that OS/arch), or carry a **known-broken build** for one specific version. Without a per-platform lever, the only knob is the global `versions.min`/`max`, which moves the window for *all* platforms at once — so a single broken `(version, platform)` either reds the run forever or forces a global version bump that strands the other platforms.

`min_version`, `max_version`, and `exclude` constrain *which versions a platform applies to*. A `(version, platform)` pair outside a platform's window — or matched by an `exclude` entry — is never resolved, scheduled, built, tested, or pushed, and never reds the run. This supersedes the old workaround of bumping the global `versions.min` to dodge a late-added or dropped platform.

```yaml
platforms:
  windows/arm64:
    runner: windows-11-arm
    shell: pwsh
    min_version: "0.11.7"          # platform's first upstream release (inclusive)
    exclude:
      - version: "0.16.0"          # one known-broken release
        reason: "aarch64-windows build-exe segfault"
        severity: broken           # 🔒 row in the Discord report (default)

  darwin/amd64:
    runner: macos-14
    max_version: "11.1.0"          # dropped upstream at 11.1.0 (exclusive)
    exclude:
      - max_version: "9.4.0"       # never built anything below 9.4.0
        severity: skip             # silent — no 🔒 row
```

**`exclude` entry fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `version` | string | One of `version` / range | Exclude exactly this version. Mutually exclusive with `min_version`/`max_version`. |
| `min_version` | string | One of `version` / range | Inclusive lower bound of an excluded range. |
| `max_version` | string | One of `version` / range | Exclusive upper bound of an excluded range. A range may set either bound alone (open-ended). |
| `reason` | string | No | Surfaced in the 🔒 row for `broken` excludes. |
| `severity` | `broken` \| `skip` | No | `broken` (default) drops the pair and surfaces a 🔒 row (plus `reason`); `skip` drops it silently. |

**Semantics:**

- `min_version` is inclusive, `max_version` is exclusive — the same convention as the top-level `versions` bounds.
- An `exclude` entry must set either a single `version` **or** a `min_version`/`max_version` range, not both.
- To re-enable a previously-excluded pair, delete the entry — the next clean run backfills it.
- Validation rejects unparseable bounds and conflicting `exclude` shapes with exit code 65 (`DataError`).

**Container shell defaults:**

- `alpine*` → `sh`
- `ubuntu*`, `debian*`, `fedora*`, `rocky*`, `opensuse*` → `bash`
- Any other image: `shell` is required.

## `ocx_mirror` {#ocx-mirror}

Records which `ocx-mirror` produced a plan. It pins nothing — see the box below for where the binaries actually come from.

```yaml
ocx_mirror:
  rev: abc123def0123456789012345678901234567890
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `rev` | string | No | Full 40-character git SHA, echoed back as `ocx_mirror_rev` in `pipeline plan` output for traceability. Must match `^[0-9a-f]{40}$`. |

!!! info "Where the binaries come from"
    Generated jobs install the toolchain via the [`ocx-sh/setup-ocx`][setup-ocx] action, which activates the mirror repository's project toolchain (`ocx.toml` / `ocx.lock`) onto `PATH` — `ocx-mirror` and `ocx` both come from there. Container test legs additionally download a statically-linked `ocx` release; that tag is a constant in the renderer, not a spec field, so the whole fleet tests against one binary and it advances when the repository's pinned `ocx-mirror` does.

## `notify` {#notify}

Configures [Discord][discord] webhook notifications. The webhook fires after the push job completes.

```yaml
notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "123456789012345678"
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `discord.webhook_secret` | string | Yes (when `notify:` is present) | Name of a [GitHub Actions secret][github-actions-secrets] whose value is the Discord webhook URL. Must match `^[A-Z][A-Z0-9_]+$`. |
| `discord.user_id` | string | No | Discord user ID ([snowflake][discord-snowflake]) to mention on failures. Non-secret — inlined into the workflow as `OCX_MIRROR_DISCORD_USER_ID`. Must match `^[0-9]{17,20}$`. |

**Validation:**

- `webhook_secret` must be a secret name, not a URL. Values containing `discord.com`, `discordapp.com`, or matching `^https?://` are rejected at parse time with exit code 64 (`UsageError`). This prevents accidental commit of a live webhook URL into the repository.
- `user_id` must be the numeric snowflake. A URL or `@mention` paste is rejected with exit code 64 (`UsageError`); any other malformed value is rejected with exit code 65 (`DataError`).

**Messages:**

The report posts **one Discord message per published version** — a single embed each (so a release-heavy run never trips Discord's 1024-character field cap, and each release reads as its own notification). Consecutive messages are paced and a `429 Too Many Requests` is retried per Discord's `retry_after`, so a large backfill stays under the webhook rate limit. Each embed lists that version's platforms with a status chip:

| Chip | Meaning |
|------|---------|
| 🟢 | Pushed |
| 🔴 | Test or push failure |
| 🚫 | Expected artifact never arrived (missing bundle / JUnit) |
| 🔒 | Deliberately excluded for this version (a `broken` [`exclude`](#platform-version-applicability) entry), with its reason |

When `user_id` is set, any message that carries a partial or failed version is prefixed with an in-message `<@id>` mention — scoped to that one user, so `@everyone` and role pings never fire. Messages with only successful versions never ping.

**Notification conditions:**

| Condition | Action |
|-----------|--------|
| All versions already existed in the registry, no failures | Silent (no POST sent) |
| New versions published, no failures | Green per-version embeds with published platforms; no mention |
| New versions published, some platforms failed | Yellow/red embeds for the affected versions; mention if `user_id` set |
| No new versions published, all platforms failed | Red embeds with failure details and run URL; mention if `user_id` set |

## `annotations` {#annotations}

[OCI annotations][oci-annotations] written onto the image index of every tag a push writes — the version tag and each rolling cascade tag alike.

Two keys are filled in automatically from the workflow environment, so a mirror running in GitHub Actions needs no configuration at all:

| Key | Value |
|-----|-------|
| `org.opencontainers.image.source` | `$GITHUB_SERVER_URL/$GITHUB_REPOSITORY` — the mirror repository itself |
| `org.opencontainers.image.revision` | `$GITHUB_SHA` |

`image.source` names the **mirror** repository, not the upstream project. Registries use this key for package-to-repository linkage: on [GHCR][ghcr-source] it is what attaches the package to a repository and lets it inherit that repository's permissions, so it has to name a repository the publisher controls.

Outside CI the variables are absent and the annotations are simply not written — `ocx package push` then leaves whatever the registry already holds untouched, so a local run never clears a link an earlier CI run established.

The `annotations:` block adds further keys and overrides an auto-detected one:

```yaml
annotations:
  org.opencontainers.image.licenses: Apache-2.0
  org.opencontainers.image.vendor: OCX
```

A key listed here replaces the auto-detected value for **that key only**; the others still apply. Values are taken verbatim — nothing is read from the environment beyond the three variables above (see [Variables read by ocx-mirror][env-read]).

**Validation:**

- A key must be non-empty and must not contain `=`. Annotations reach `ocx package push` as `--annotation KEY=VALUE`, so a `=` in the key would be re-split at the wrong place and publish a different key than configured. Violations are rejected with exit code 65 (`DataError`).

## `announce` {#announce}

Publishes this mirror's tags into the [OCX index][index-repo] after a push run. Opt-in — without an `announce:` block nothing is announced.

```yaml
announce:
  package: bazelbuild/bazelisk
  fork: ocx-contrib/index
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `package` | string | Yes | Logical index package as `<namespace>/<package>`. Not derived from [`target.repository`](#target) — the physical path and the logical name are related by convention only. |
| `fork` | string | Yes | Fork the index pull request is opened from, as `<owner>/<repo>`. |
| `index_repo` | string | No | Index repository the pull request targets, as `<owner>/<repo>`. Defaults to `ocx-sh/index`. |

**Behaviour:**

The push job makes **one** `ocx package announce` call per run, after every version has been pushed — never one per version or per platform. It carries the union of every cascade tag the run wrote, deduplicated: each platform's push report re-lists the same cascade hierarchy, and consecutive versions share the rolling `X.Y` / `X` / `latest` tags. Versions that only failed, or that were already present in the registry, contribute nothing.

Tags are handed over with `--tags-from-file`, which **adds** to the already-curated index entry and never removes a committed tag. The alternative, `--tags`, replaces the curated set — for a mirror that would delete every previously announced version the moment one run published a new one.

A run that published nothing makes no call at all.

**Partially published versions:**

A rolling alias — `latest`, `X`, `X.Y` — means "the best build of this line", so it has to resolve to a complete platform set. A version any platform of which failed never gets one: the push job decides every `(version, platform)` pair *before* it pushes anything, and passes `--cascade` only once every platform of that version is green. The green platforms of a partial version publish under the **exact version tag** `X.Y.Z` alone.

When the version is whole, **every** one of its pushes carries `--cascade` — not just the last. A cascade push merges its own platform into each rolling tag and leaves every other platform's entry on that tag exactly as it found it, so cascading once per version would strand the remaining platforms on `X.Y.Z` and leave each alias still pointing at the *previous* version for them. `latest` would become a mixed-version index and those platforms would never advance.

So a partial version announces `X.Y.Z` and nothing else — not because the announce filters aliases, but because the registry never received any. Filtering them at announce time cannot work: `ocx package announce` re-observes every tag the index entry already curates, so an alias an earlier run committed is re-fetched from the registry and re-committed against whatever it points at *now*. Withholding an alias only ever blocks its first addition, and an established mirror already has all of them.

Three gaps remain, all narrower than the registry write they replace:

- A version already published by an earlier run keeps whatever aliases that run wrote. Nothing here retracts them, and neither does the catch-up workflow below — it is additive. Retracting an alias needs a manual `ocx package push --cascade` of a whole version, or a manual `ocx package announce --tags`.
- A platform the workflow never built a bundle for is invisible to the push job, which sees only the bundles that arrived. A version whose `prepare` leg failed outright can therefore still look whole.
- A version decided whole whose *push* then fails part-way leaves the aliases carrying the platforms that landed before the failure, and the previous version for the rest. The remaining platforms are withheld from cascading the moment the failure is seen, but a registry write already made cannot be taken back. Re-running the version repairs it.

`run-summary.json` reports the tags the registry actually received. `cascade_tags_written` for a partial version holds only `X.Y.Z` because that is all that was written.

**Credentials:**

The announce needs an `OCX_ANNOUNCE_TOKEN` [secret][github-actions-secrets] with push access to the fork and permission to open the pull request. Generated workflows thread it into the push step's environment.

Without the secret the run still pushes and still reports its results; the announce is skipped, a GitHub notice is emitted, and `run-summary.json` records it:

| `announce.status` in `run-summary.json` | Meaning |
|------|---------|
| *(key absent)* | No `announce:` block — the mirror never opted in |
| `announced` | Index pull request opened or updated, with the tags listed under `tags` |
| `nothing_to_announce` | Configured, but the run produced no new tag |
| `skipped_no_credential` | Configured, but no `OCX_ANNOUNCE_TOKEN` — a valid configuration for forks and test repos |
| `failed` | The call ran and failed, with the detail under `error` |
| `interrupted` | The run was killed while the announce was in flight — a reclaimed runner, a cancelled backfill. Whatever pushed is live in the registry and the index state is unknown. |

`interrupted` is written *before* the announce runs and overwritten by whichever of the others it reaches. Its presence, rather than an absent key, is the signal: an absent key already means "this mirror has no `announce:` block", and a killed run must not read as one that never opted in.

**Exit code and job output:**

`failed` **fails the push job**, on the same reasoning as a red platform: the images are in the registry and the index does not know about them. Left green, an expired `OCX_ANNOUNCE_TOKEN` keeps every nightly passing while the index drifts arbitrarily far behind the registry, and no scheduled-run alert ever fires because nothing failed. `skipped_no_credential` does not fail the job — a mirror without the secret is a valid configuration.

The push job exports the outcome as an `announce` job output, so `notify` and any branch protection can branch on it. Its value is the `announce.status` above, or `unconfigured` when the mirror has no `announce:` block — plus `not_run` when the push step itself was skipped for lack of registry credentials.

Whichever of these a run lands on is also rendered as an **Index** row on the run's Discord notification, so a skipped, failed or interrupted announce cannot look like a successful one.

**Catching up an existing mirror:**

The announce only ever carries what the *current run* published, so adding `announce:` to a mirror that has already published everything reports `nothing_to_announce` on every run, indefinitely — there is nothing new to trigger it. The same applies after an announce failure: the next run has nothing to retry with.

Every mirror with an `announce:` block gets a second generated workflow, `announce-from-registry.yml`, for exactly this. It is `workflow_dispatch` only — never scheduled, never triggered by a push — and it lists every tag the target repository currently holds, then unions them onto the committed index entry. Dispatch it from the repository's **Actions** tab, or:

```sh
gh workflow run announce-from-registry.yml --repo <owner>/<mirror> -f dry_run=false
```

`dry_run` defaults to **true**: the run reports whether the index would change (`updated` or `unchanged`) and discards the rebuilt entry without opening a pull request. Pass `dry_run=false` to open it for real.

The catch-up is **additive**, on the same footing as the push job's `--tags-from-file`: it cannot drop a tag the index already commits, and yank markers survive. Running it against a mirror that is already current is a no-op, so it is safe to dispatch on suspicion.

Its `ocx-mirror` entry point is [`pipeline announce`][cli-announce]; the same command runs locally against a checkout.

(`--refresh` on `ocx package announce` solves a different problem — it re-observes the tags already committed, picking up a digest that moved, and never adds one.)

**Validation:**

- `package` must be a `<namespace>/<package>` pair of lowercase alphanumerics with `.`, `_` or `-`. A bare tool name is rejected with exit code 65 (`DataError`).
- `fork` and `index_repo` must each be an `<owner>/<repo>` pair. A pasted URL is rejected the same way.

## `catalog` {#catalog}

Configures the README and logo [`pipeline describe`][cli-describe] publishes to the target registry as the `__ocx.desc` referrer tag. Optional — omit the block and the defaults below apply.

```yaml
catalog:
  readme: docs/catalog.md
  logo: brand/logo.png
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|--------------|
| `readme` | string | No | Path to the README, relative to the spec's own directory. Defaults to `CATALOG.md`. |
| `logo` | string | No | Path to the logo, relative to the spec's own directory. When unset, the resolver probes `logo.svg` then `logo.png` in that same directory — SVG wins when both exist. |

Both paths resolve against the directory holding the spec file, never the repository root — see [`catalog:` resolves from the spec's own directory](#multi-spec-catalog-path) for where that matters in a multi-spec repository.

**Validation:**

- `deny_unknown_fields` — a key other than `readme` or `logo` under `catalog:` is a spec-load failure (exit 65), not a silently-ignored typo.

When the resolved README does not exist on disk, `pipeline describe` logs and exits 0 — the workflow is a no-op until catalog content lands in the repository.

## Spec inheritance {#inheritance}

`mirror.yml` files support an `extends:` key for shallow merge from a parent spec. Child keys override parent keys at the top level. This is useful for sharing `source` and `assets` across variants of the same tool.

```yaml
extends: ./base-cmake.yml
target:
  registry: private.registry.example.com
  repository: internal/cmake
```

A base is part of its children's effective content, so [`pipeline generate ci`][cli-generate-ci] adds every file in the `extends:` chain to the `paths:` trigger of each child's generated workflows — editing a shared base re-runs every package that inherits from it, and the drift guard covers it too.

**The chain must live inside the repository.** A base above the repository root is a spec-usage error (exit 64): a `paths:` trigger can only name files the workflow's own repository contains, so the workflow would silently never run when that base changed.

## Multi-spec repositories {#multi-spec}

A mirror repository can hold more than one `mirror.yml` — one per package, each in its own directory. Some upstream projects release several standalone tools from a single tag: [bazelbuild/buildtools][bazelbuild-buildtools] ships `buildifier`, `buildozer`, and `unused-deps` from one release. Mirroring each as its own repository would triplicate the CI plumbing — and the drift guard, and the secrets — for tools that share one upstream release cadence. Putting each package's spec in its own directory and passing `--spec` once per spec keeps them in one repository: [`pipeline generate ci`][cli-generate-ci] renders an independent workflow set per spec and exactly one drift guard for the whole repository.

**Repository layout.** `ocx-contrib/mirror-bazelbuild` already mirrors `bazelisk` from a `mirror.yml` at its root. Adding the three `buildtools` binaries means one directory per package, each holding its own `mirror.yml` and `CATALOG.md`, sharing the repository's one logo:

```
mirror-bazelbuild/
├── mirror.yml                # bazelisk — stays at the repo root, unchanged
├── logo.svg                  # shared by every spec in the repository
├── buildifier/
│   ├── mirror.yml
│   └── CATALOG.md
├── buildozer/
│   ├── mirror.yml
│   └── CATALOG.md
└── unused-deps/
    ├── mirror.yml
    └── CATALOG.md
```

```sh
ocx-mirror package pipeline generate ci \
  --spec mirror.yml \
  --spec buildifier/mirror.yml \
  --spec buildozer/mirror.yml \
  --spec unused-deps/mirror.yml
```

writes:

```
.github/workflows/
├── mirror.yml                            # bazelisk — byte-identical to before
├── describe.yml
├── announce-from-registry.yml
├── mirror-buildifier.yml
├── describe-buildifier.yml
├── announce-from-registry-buildifier.yml
├── mirror-buildozer.yml
├── describe-buildozer.yml
├── announce-from-registry-buildozer.yml
├── mirror-unused-deps.yml
├── describe-unused-deps.yml
├── announce-from-registry-unused-deps.yml
└── verify-generated.yml                  # one guard, names all four specs
```

Naming a nested spec file `mirror.yml` is convention, not a requirement — the generated filenames derive from the spec's **directory**, never its filename (below). Keep the filename anyway: it matches every other spec in the repository, and it is the directory — not the name — that `--repo-root`'s default and the collision check both reason about.

**Generated file names.** A spec at the repository root keeps today's filenames byte for byte — `mirror.yml`, `describe.yml`, `announce-from-registry.yml` — so a repository that adds its first nested spec never has to touch the workflows it already published. A spec anywhere else gets every filename suffixed with its directory, `/` joined by `-`:

| Spec path (relative to repo root) | Suffix | `mirror.yml` becomes |
|---|---|---|
| `mirror.yml` | *(none)* | `mirror.yml` |
| `buildifier/mirror.yml` | `-buildifier` | `mirror-buildifier.yml` |
| `a/b/mirror.yml` | `-a-b` | `mirror-a-b.yml` |

Because the suffix comes from the directory alone, **a directory may hold only one spec** — two specs sharing a directory, whatever their filenames, would render the same workflow set and silently overwrite each other. `generate ci` rejects this with exit 64 before writing anything.

Every generated pipeline invocation in a nested spec's workflows names its own spec explicitly — `pipeline plan --spec buildifier/mirror.yml`, and likewise for `prepare`, `push`, `describe`, `announce`. The root spec's invocations never carry `--spec`: its path is exactly what every subcommand already defaults to, which is what keeps the root workflows byte-identical.

**`--repo-root`.** Generated files are written under `--repo-root`, and every filename above is computed relative to it. Left unset, it defaults to the deepest directory every `--spec` given shares — for a single spec that is simply its parent directory, so `generate ci --spec /elsewhere/repo/mirror.yml` still writes into that repository rather than the current directory. A spec that does not resolve under `--repo-root` (explicit or inferred) is rejected with exit 64, naming `--repo-root` as the fix.

**CI triggers per spec.** The root spec's workflow keeps the repository-wide trigger list it has always had (its own spec file, `scripts/**`, `tests/**`, `metadata*.json`) plus its own workflow file. A nested spec's workflow instead triggers only on its own subtree — `buildifier/**` plus `.github/workflows/mirror-buildifier.yml` — never the repository-wide list, so editing `buildozer/` never wakes `buildifier`'s workflow. The generated `describe-<dir>.yml` and `announce-from-registry-<dir>.yml` follow the same rule for their own triggers (`CATALOG.md` / `logo.*` at the root, `<dir>/**` when nested), and each carries a distinct `name:` — sibling `describe` workflows sharing a name would share a `concurrency.group` too, since it keys on `github.workflow`.

**One drift guard per repository.** `verify-generated.yml` is emitted once no matter how many specs the repository holds. Its committed `paths:` list is the union of every spec's own triggers, and the `generate ci --check` command line it bakes in names every spec explicitly with `--spec` as soon as there is more than one — `--spec` appends rather than replaces, so a guard naming only a subset would silently stop checking the rest while staying green. The guard also reds when a `.github/workflows/*.yml` file carries the `# Generated by ocx-mirror` header but is not in the current spec set's output — the file a dropped spec leaves behind, which would otherwise keep running on schedule against a spec that no longer exists. Hand-written workflows without that header are never inspected.

`allow_manual_edits: true` disarms the guard only when **every** spec in the repository sets it; a partial opt-out still emits the guard — covering every spec, including the ones that opted out — and `generate` prints a warning naming the dissenters.

### `script:` resolves from the repository root {#multi-spec-script-path}

A [`tests`](#tests) entry's `script:` path is read relative to where the workflow checks the repository out — the repository root — never the spec's own directory. In a single-spec repository the two coincide, so the distinction is invisible. In a multi-spec repository it is not: `buildifier/mirror.yml` must write

```yaml
tests:
  - name: smoke
    script: buildifier/tests/smoke.star
```

not `tests/smoke.star`. [`metadata.default`](#metadata) and [`catalog.readme` / `catalog.logo`](#catalog) work the other way — both resolve against the spec's own directory — so the same-looking relative path means something different depending on which key it sits under. The nested workflow's own trigger-path comment says as much, so the gap is visible without opening this page:

```yaml
    paths:
      # `script:` paths resolve from the repository root, not from buildifier/ —
      # keep this spec's scripts under buildifier/ so editing one triggers this run.
      - buildifier/**
      - .github/workflows/mirror-buildifier.yml
```

[`pipeline generate ci`][cli-generate-ci] rejects a `script:` that resolves to nothing (exit 65, like a missing [`metadata.default`](#metadata)) — top-level and per-platform entries alike. When the file exists where a spec-directory-relative reading would put it, the error says so and names the path to write instead.

### `catalog:` resolves from the spec's own directory {#multi-spec-catalog-path}

[`catalog.readme` and `catalog.logo`](#catalog) resolve against the directory holding the spec file — the opposite of `script:` above. A logo shared at the repository root is invisible to a nested spec's default probe (`logo.svg`, then `logo.png`, looked for only in `buildifier/`), so every nested spec that wants the shared logo has to say so explicitly:

```yaml
# buildifier/mirror.yml
catalog:
  logo: ../logo.svg
```

`catalog:` is `deny_unknown_fields`, so a key from the wrong block — `default:` where `readme:` was meant, for instance — is a spec-load failure (exit 65), not a silently-ignored typo.

## Example: complete spec {#example}

```yaml
name: cmake
target:
  registry: ocx.sh
  repository: cmake

source:
  type: github_release
  owner: Kitware
  repo: CMake
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"

assets:
  linux/amd64:
    - "cmake-.*-linux-x86_64\\.tar\\.gz$"
  darwin/arm64:
    - "cmake-.*-macos-universal\\.tar\\.gz$"
  windows/amd64:
    - "cmake-.*-windows-x86_64\\.zip$"

cascade: true

tests:
  - name: version
    command: cmake --version
  - name: ctest
    command: ctest --version

platforms:
  linux/amd64:
    runner: ubuntu-latest

  darwin/arm64:
    runner: macos-latest

  windows/amd64:
    runner: windows-latest
    shell: pwsh
    min_version: "3.20.0"          # cmake windows/amd64 mirrored from 3.20 on
    exclude:
      - version: "3.27.0"
        reason: "windows zip repacked upstream"
        severity: broken
    tests:
      - name: version
        command: cmake.exe --version

notify:
  discord:
    webhook_secret: DISCORD_WEBHOOK_URL
    user_id: "123456789012345678"
```

<!-- external -->
[github-releases]: https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases
[github-actions-docs]: https://docs.github.com/en/actions/writing-workflows/choosing-what-your-workflow-does/using-pre-written-building-blocks-in-your-workflow
[github-actions-secrets]: https://docs.github.com/en/actions/security-for-github-actions/security-guides/using-secrets-in-github-actions
[discord]: https://discord.com/developers/docs/resources/webhook
[discord-snowflake]: https://discord.com/developers/docs/reference#snowflakes
[setup-ocx]: https://github.com/ocx-sh/setup-ocx
[oci-annotations]: https://github.com/opencontainers/image-spec/blob/main/annotations.md
[ghcr-source]: https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry#labelling-container-images
[index-repo]: https://github.com/ocx-sh/index
[starlark]: https://github.com/bazelbuild/starlark
[bazelbuild-buildtools]: https://github.com/bazelbuild/buildtools

<!-- internal -->
[env-read]: ./environment.md#annotation-env

<!-- commands -->
[cmd-pipeline]: ./cli.md#pipeline
[cmd-sync]: ./cli.md#sync
[cli-announce]: ./cli.md#pipeline-announce
[cli-generate-ci]: ./cli.md#pipeline-generate-ci
[cli-describe]: ./cli.md#pipeline-describe
[cli-patch]: ./cli.md#pipeline-patch
