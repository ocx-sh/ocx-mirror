# mirror.yml Reference

`mirror.yml` describes one tool to mirror — where to fetch upstream releases, which platforms to build for, how to test each bundle, and how to report results. The file is consumed by `ocx-mirror package sync`, `ocx-mirror package check`, and all `ocx-mirror package pipeline` subcommands.

## Top-level keys {#top-level}

| Key | Type | Required | Purpose |
|-----|------|----------|---------|
| `name` | string | Yes | Tool name, used in log output and notify messages |
| `target` | object | Yes | OCI registry and repository to push to |
| `source` | object | Yes | Upstream release source: [GitHub Releases][github-releases], URL index, a committed [PEP 751 `pylock.toml`](#pylock), or an [index-discovered PyPI package](#pypi-source) |
| `assets` | object | Yes* | Platform → regex list mapping for selecting upstream release archives. *Mutually exclusive with `variants` — exactly one of the two required for `github_release`/`url_index` sources. Not used by `source.type: pylock`/`pypi` (see `wheels`). |
| `variants` | array | No* | Alternate asset sets for the same tool (per-variant `assets`/`metadata`/`asset_type`), each producing its own version-tag prefix. *Mutually exclusive with `assets` — exactly one of the two required for `github_release`/`url_index` sources; rejected for env sources (`pylock`/`pypi`). See [`variants`](#variants). |
| `metadata` | object | No | Path(s) to the package metadata JSON, with optional per-platform overrides. See [`metadata`](#metadata). |
| `bin_scan` | string | No | `off` (default), `auto`, or `verify` — derive the published `binaries` claim from the extracted bundle. See [`bin_scan`](#bin-scan). |
| `libc_lint` | boolean | No | Check a Linux build's declared `os.features` against the libc its binaries link against (`true` by default). See [`libc_lint`](#libc-lint). |
| `asset_type` | string | No | `Archive` (default) or `Binary`. Not used by `source.type: pylock`/`pypi`. |
| `python` | object | No* | Interpreter version/ABI + `interpreter_package`, plus optional [`lock`](#python-lock) and [`entrypoints`](#entrypoints) config. **Required** for `source.type: pylock` or `pypi`. See [Python apps](#pylock). |
| `wheels` | object | No* | Per-platform wheel selection for env sources. **Required** for `source.type: pylock`/`pypi`; keys may carry `+libc.glibc`/`+libc.musl` (published as OCI `os.features`). See [`wheels`](#wheels). |
| `wheel_scope` | string | No | Repo-naming scope prefix for [shared wheel layers](#shared-wheel-layers) (`source.type: pylock`/`pypi`). Default `pip-packages`. |
| `build_timestamp` | string | No | Per-build tag suffix: `datetime` (default), `date`, or `none`. See [build_timestamp & GC-safe publishing](#build-timestamp). |
| `cascade` | boolean or object | No | Cascade rolling tags on push (`true` by default), and optionally put the generated repair workflow on a timer. See [`cascade`](#cascade). |
| `versions` | object | No | Version filter (min/max bounds, `new_per_run`, backfill order) |
| `verify` | object | No | Checksum verification options |
| `concurrency` | object | No | Parallel download limits, source rate limiting, push retry policy. See [`concurrency`](#concurrency). |
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
<os>/<arch>[/<variant>][+libc.<flavor>[,...]]
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
## Python apps (`source.type: pylock` / `pypi`) {#pylock}

A `pylock` or `pypi` source mirrors a Python **application** into a runnable OCX **environment package** — the union of every resolved wheel plus a private interpreter, composed so it runs via `ocx run` on a clean machine with **no pip, uv, or venv at runtime**. This replaces the `assets`/`asset_type` archive model (both source types ignore both fields). The two types differ only in where the [PEP 751](https://peps.python.org/pep-0751/) lock comes from:

- **`pylock`** — a lock file committed to the mirror repository; resolves exactly one version (the one recorded in the lock).
- **`pypi`** — versions are discovered from a PyPI-compatible index, and a lock is derived in-pipeline per version (see [`source.type: pypi`](#pypi-source)).

Everything downstream of "a lock is in hand" — wheel selection ([`wheels`](#wheels)), entrypoint synthesis ([`python.entrypoints`](#entrypoints)), composition, and [shared wheel layers](#shared-wheel-layers) — is identical for both.

```yaml
name: black                       # PEP 503-normalized to match the app package in the lock
target:
  registry: dev.ocx.sh
  repository: ocx/black
source:
  type: pylock
  path: black.pylock.toml         # repo-relative path to the PEP 751 lock
python:
  version: "3.14.6"               # interpreter version
  abi: cp314                      # target ABI tag
  interpreter_package: "ocx.sh/cpython:3.14.6"   # OCX package providing the interpreter
wheels:
  "linux/amd64+libc.glibc":       # glibc entry (default filter [manylinux, any])
tests:
  - name: smoke
    script: tests/black.smoke.star
platforms:
  linux/amd64:
    runner: ubuntu-latest
```

### `source.type: pypi` — index-discovered apps {#pypi-source}

A `pypi` source discovers upstream versions directly from a PyPI-compatible index instead of a committed lock file — useful for apps whose releases you want to track automatically rather than re-lock and commit by hand.

```yaml
name: pycowsay
target:
  registry: dev.ocx.sh
  repository: ocx/pycowsay
source:
  type: pypi
  package: pycowsay                # PEP 503 name on the index; defaults to `name`
  index: https://pypi.org          # optional; Warehouse-compatible JSON API base
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/cpython:3.13.1"
  lock:
    universal: true                 # see python.lock below
platforms:
  linux/amd64:
    runner: ubuntu-latest
```

**`source` fields (`type: pypi`):**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `package` | string | No | PEP 503 name of the PyPI package to resolve. Defaults to the mirror's `name`. |
| `index` | string | No | Warehouse-compatible index base URL — versions are read from `GET {index}/pypi/<package>/json`. Must be `http`/`https`. Default: `https://pypi.org`. |

**Discovery semantics:**

- A release is listed only when it has at least one file that is not [yanked (PEP 592)](https://peps.python.org/pep-0592/); a release with zero files, or with every file yanked, is dropped entirely.
- Prerelease detection is PEP 440-aware (`uv_pep440`), not the mirror's own semver-ish version parser — a `2.0.0.dev0` release is correctly flagged as a prerelease and respects the existing `skip_prereleases`/`versions` bounds the same as any other source.
- An index that returns 404 for the package name is a data error (malformed input — the package doesn't exist on that index, exit code 65), not an availability failure; any other failure (connection refused, timeout, 5xx, malformed JSON) stays a source-unavailable error (exit code 69).

Per-version lock derivation (running `uv pip compile`) happens later, in `pipeline plan` — see [`python.lock`](#python-lock) and [`--locks-dir`](#python-lock). A universal lock (the default) resolves via `--python-version` alone; only `universal: false` materializes the pinned `interpreter_package` on disk to resolve against it.

### How the app is resolved

The lock lists every package in the resolved environment; `ocx-mirror` picks the one whose name **PEP 503-normalizes** (lowercase, runs of `-_.` → `-`) to the spec's `name` as *the app*, and mirrors its locked version. So a `[full]`-extras distribution keeps its distribution name: `name: google-cloud-aiplatform` (not `aiplatform`). A `name` that matches no locked package fails with exit 65. For `pypi`, the same `source.package`/`name` fallback selects which index package to resolve — there is no committed lock to cross-check the app name against until one is derived.

Set `source.package` to resolve a *different* app name than the mirror `name` — e.g. a `pycowsay-musl` mirror (distinct target repo + workflow) that resolves the `pycowsay` package from a shared lock:

```yaml
source:
  type: pylock
  path: pycowsay.pylock.toml
  package: pycowsay               # resolve this package; defaults to the mirror name
```

### `python` block

Required for `source.type: pylock` or `pypi`. Fields:

| Key | Purpose |
|-----|---------|
| `version` | Interpreter version (e.g. `3.14.6`). Feeds the PEP 508 marker environment used for wheel selection. |
| `abi` | Target ABI tag (e.g. `cp314`). Every compiled wheel's ABI must match this (or be `abi3`/`none`), checked fail-closed at compose. |
| `interpreter_package` | An OCX package that provides `python3` (a [python-build-standalone](https://github.com/astral-sh/python-build-standalone) build). Pulled in as a **private dependency** and pinned by digest; its platform-agnostic index digest is resolved per-platform at materialize. |
| `lock` | Lock-derivation options — `source.type: pypi` only. See [`python.lock`](#python-lock). |
| `entrypoints` | Which console scripts synthesize as OCX entrypoints. Default `auto`. See [`python.entrypoints`](#entrypoints). |

### `python.lock` — pypi lock derivation {#python-lock}

`source.type: pypi` has no committed lock, so `ocx-mirror` derives one per version in-pipeline (`pipeline plan`, via `uv pip compile`). `python.lock` configures that derivation; it is meaningless for `source.type: pylock` (a committed lock is already resolved) and is rejected there with exit code 65: `python.lock: only supported for source.type 'pypi' (a committed lock is already resolved)`.

```yaml
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/cpython:3.13.1"
  lock:
    universal: true            # default: true
    extras: []                 # default: []
    exclude: []                # default: []
    timeout_seconds: 300       # default: 300
```

| Field | Type | Default | Description |
|-------|------|---------|--------------|
| `universal` | boolean | `true` | Resolve a platform/interpreter-agnostic universal lock (`uv pip compile --universal`) rather than one pinned to the resolving host. |
| `extras` | array of strings | `[]` | Extras to include when resolving the lock (e.g. `["full"]` for `app[full]`). |
| `exclude` | array of strings | `[]` | Package names to exclude from resolution (`uv --no-emit-package`). |
| `timeout_seconds` | integer | `300` | Timeout for the `uv pip compile` subprocess. |

Each derived lock is written under `--locks-dir` (a `pipeline plan` flag, default `./locks`, relative to the command's working directory — the same directory `plan.json` is written to) as `pylock.<package>-<version>.toml`, with a relaxed `requires-python` floor (works around a known `uv` over-strict-patch-pin issue) and a provenance comment header. `pipeline prepare --plan` reads the path straight from the plan instead of re-deriving; a standalone `pipeline prepare` (no `--plan`) re-derives it from scratch.

Every dot in the `<package>-<version>` segment becomes a dash (`pylock.black-26-5-1.toml`): `uv` enforces PEP 751 on its `-o` argument, where the name between `pylock.` and `.toml` must be non-empty and dot-free. Nothing parses the name back — each plan entry's `pylock` field carries the full path — so the substitution is safe.

A `uv` resolution failure (unsolvable requirements, bad package metadata) is a data error, exit 65 — the version cannot produce a trustworthy lock. A missing/unspawnable `uv` binary, a timeout, or lock-file I/O failure is a subprocess execution failure, exit 1.

### `python.entrypoints` {#entrypoints}

Controls which wheels' `[console_scripts]` entries synthesize as OCX entrypoints in the composed env.

| Value | Behavior |
|-------|----------|
| `auto` (default) | Only the **root package's** own console scripts synthesize (root = `source.package`/mirror `name`). **New default** — previously every wheel's scripts synthesized unconditionally. |
| `all` | Every wheel's console scripts synthesize — the pre-`auto` behavior. |
| explicit list | Only the listed console-script names synthesize, each optionally windowed to an app-version range. |

```yaml
python:
  entrypoints: auto   # or: all

# or an explicit, version-windowed list:
python:
  entrypoints:
    - name: black
    - name: blackd
      min_version: "24.0.0"   # inclusive
      max_version: "25.0.0"   # exclusive
```

`min_version`/`max_version` follow the same inclusive-lower/exclusive-upper convention as `versions:` and per-platform bounds; an entry with neither is unbounded. An app version that fails to parse keeps every explicit entry (fail-open, same convention as platform excludes).

**Fails closed** in two cases, both surfaced as a compose/pylock error (exit 65):

- **Collision** — two different wheels register a console script under the same entrypoint name and the selection mode admits both (only possible under `all`, or an `explicit` name two wheels both provide).
- **Miss** — an `explicit` name that no admitted wheel's console scripts actually provide.

**Nuance — `auto` removes dependency console-script shims.** Under `auto`, a *dependency* wheel's own console script (e.g. a library the app depends on that ships its own CLI) no longer synthesizes as an entrypoint. If the app itself spawns that dependency's CLI as a subprocess (`subprocess.run(["some-dep-cli", ...])`), the spawn will fail to find it under `auto` — such an app needs `all`, or the dependency's script name listed explicitly.

### `wheels` — per-platform wheel selection {#wheels}

Env sources declare their support envelope in a top-level `wheels:` map — the env analogue of the archive `assets:` map. It is **required** for `source.type: pylock`/`pypi` and rejected for every other source; `variants:` is rejected outright for env sources (libc is a platform `os.features` axis for env packages, never a variant/tag axis).

```yaml
wheels:
  linux/amd64:                          # value omitted → key-derived default filter
  "linux/arm64+libc.glibc": ~           # glibc-stamped entry
  "linux/arm64+libc.musl": [musllinux, any]   # explicit filter
  darwin/arm64: ~
  windows/amd64: ~
```

**Keys** are OCI platform strings — a concrete `os/arch`, optionally carrying **one** `+libc.glibc`/`+libc.musl` suffix (Linux only; no OCI `variant`/`os_version` segments, no other feature namespaces). The key is published **verbatim** as the image-index platform entry: a `+libc.*` suffix lands in OCI `os.features`, which ocx ≥ 0.4.2 clients match against the host libc at install time. Two keys sharing one base (`linux/amd64+libc.glibc` + `linux/amd64+libc.musl`) publish **two entries in one index under one bare tag** — one package, no variant-prefixed tags, no per-variant repos.

**The key is a declaration, not a computation.** The mirror stamps nothing and infers nothing from wheel contents. A maintainer may legitimately publish glibc-only wheels under a plain `linux/amd64` key with an explicit `[manylinux, any]` filter — that key then installs on musl hosts too (its entry carries no `os.features`); whether that is correct is the maintainer's support-envelope call, and no warning is emitted.

**Values** are ordered lists of PEP 425 platform-tag *prefixes* acting as **admissibility filter + ranking**: a tag-compatible wheel whose platform tags match no listed prefix is **excluded** (fail closed — e.g. the default `["any"]` on a plain linux key errors with exit 65 if the lock demands a compiled wheel), and earlier prefixes outrank later ones among survivors. A `~`/omitted value selects the key-derived default:

| Key class | Default filter |
|-----------|----------------|
| `linux/*` (plain) | `["any"]` — pure wheels only, runs on any libc |
| `linux/*+libc.glibc` | `["manylinux", "any"]` |
| `linux/*+libc.musl` | `["musllinux", "any"]` |
| `darwin/*` | `["macosx", "any"]` |
| `windows/*` | `["win", "any"]` |

One key's filter must not mix `manylinux*` and `musllinux*` prefixes (a single env cannot need both libcs at runtime), and a `+libc.*` key's filter must not contradict its declared libc. The filter never re-admits a wheel that tag-compatibility already excluded.

**Cross-validation with `platforms:`.** Every wheels key's base `os/arch` must be declared under [`platforms:`](#platforms) (it needs a CI test leg), and every declared platform leg must be covered by at least one wheels key. `platforms:` keys stay plain — the CI matrix is a base-platform axis.

**Container gating.** At push time, a `+libc.glibc` entry is gated only by the JUnit results of glibc container legs (debian/ubuntu/… — and native runners), a `+libc.musl` entry only by musl (alpine) legs, and a featureless entry by **all** legs of its base platform (it claims to run anywhere, so everything must be green). An entry whose declared libc no test leg covers fails closed. Pair a `+libc.musl` key with an alpine container leg.

**Interpreter.** The single `python.interpreter_package` serves every wheels key — there is no per-key interpreter override. A dual-libc app therefore needs an interpreter package that itself resolves per-libc (its own index carrying `os.features` entries), or a static/musl build that runs on both.

### What is published

Each app version becomes an environment package: one content-addressed `tar.zst` layer per wheel (deterministic repack — see the [conventions ADR](https://github.com/ocx-sh/ocx-mirror/blob/main/.claude/artifacts/adr_ocx_python_conventions.md)), a composed `metadata.json` (private interpreter dependency, `PYTHONPATH`/`PATH` env, and a synthesized entrypoint per `[console_scripts]` entry). Those console-script entrypoints are the package's whole public surface: a *library* env with no console script of its own (e.g. `google-cloud-aiplatform`) publishes none. (Each launcher dispatches `python`, not `python3`: python-build-standalone ships a `python3` binary only on POSIX platforms, while `python` resolves in-package everywhere.)

### Catalog description & `metadata:` {#env-catalog}

The top-level `metadata:` key (and any per-variant `metadata:` override) is **rejected** for `source.type: pylock`/`pypi` with exit code 65:

```
metadata: not supported for source.type 'pylock' (env metadata is composed from the lock; use catalog:/CATALOG.md for the description)
```

An env package's `metadata.json` is *composed* from the resolved lock (interpreter dependency, env vars, entrypoints) — there is nothing for a hand-authored `metadata:` file to add, and it would only drift from what compose actually produces.

`pipeline describe` publishes the registry catalog description from `CATALOG.md` as usual. When no `CATALOG.md` exists on disk, it **autogenerates** one from the root package's wheel `*.dist-info/METADATA` (`Summary` as the lead paragraph, `Keywords`/`License` as trailer lines) instead of skipping — `pylock` reads the root wheel straight from its committed lock; `pypi` looks for a lock `pipeline plan` already derived under `--locks-dir` (any one is equivalent for this purpose — core metadata doesn't vary by version) and skips silently if none is reachable yet (no prior `pipeline plan` run). An on-disk `CATALOG.md` always wins over autogen.

### Shared wheel layers {#shared-wheel-layers}

Two apps that both depend on the same `numpy` wheel do not need two copies of it in the registry. Each wheel layer is pushed once to a **content-addressed repository** and then cross-repository **mounted** into every app that depends on it, instead of being re-uploaded as a private layer per app.

**Naming.** A wheel's standalone repository is `<wheel_scope>/<index-host>/<package>`, tagged with its `sha256` — e.g. `pip-packages/files.pythonhosted.org/numpy:<sha256>`. `<wheel_scope>` is the top-level `wheel_scope` spec key (default `pip-packages`); `<index-host>` groups wheels by the index they were downloaded from. The `sha256` tag is content-addressed, so every wheel of a package — however its build tag / ABI / platform differ — lands in that one repo as a distinct tag, and byte-identical wheels (e.g. an `abi3` wheel shared across CPython minors) dedupe onto a single tag. No per-wheel path segment is needed.

**Push order.** Before pushing an app's own env package, `pipeline push` registers each not-yet-published wheel layer standalone under its content-addressed reference (skipped if already present — checked via a tag-list lookup, deduped across the whole run so a wheel shared by many apps/platforms is checked once). The app's own layer positionals then each carry a `:from=<wheel_repository>` tail (`ocx package push …/wheel.tar.zst:from=pip-packages/files.pythonhosted.org/numpy`), so the push attempts a cross-repository blob **mount** against that standalone registration before falling back to a full upload on a miss — the fallback is load-bearing, not a bug.

**Visibility.** `run-summary.json` carries a `layer_reuse` counter per version, aggregated across all its pushed platforms:

| Field | Meaning |
|-------|---------|
| `mounted` | Layers reused via cross-repository mount (no re-upload) |
| `uploaded` | Layers freshly uploaded |
| `verified` | Layers already present, verified rather than re-checked |

Archive/binary mirrors have no shared-layer concept and always report all-zero counts.

### Multi-platform

Add `linux/arm64`, `darwin/arm64`, etc. to `platforms`. A **pure** app reuses one lock across all platforms. A **compiled** app needs a *universal* lock (`uv pip compile … --universal`) so each per-platform leg selects the right wheel (`manylinux_2_28_aarch64`, `macosx_11_0_arm64`, …); where no compiled wheel exists for a platform the `py3-none-any` fallback is selected.

!!! note "Overlap-free layer union"
    OCX composes the env as an overlap-free prefix-layer union, so two wheels must never install the *same* file. A valid resolved lock is collision-free by construction; a pathological `[extras]` closure that pulls mutually-exclusive distributions sharing a file (e.g. `mlflow` + `mlflow-skinny` + `mlflow-tracing`, which each ship an identical `mlflow/__init__.py`) is rejected with exit 65 — curate the lock (`uv --no-emit-package <redundant>`) to keep the superset.

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
| `bin_scan` | string | No | Overrides the top-level [`bin_scan`](#bin-scan) for this variant only. |
| `libc_lint` | boolean | No | Overrides the top-level [`libc_lint`](#libc-lint) for this variant only. |
| `asset_type` | string | No | Overrides the top-level `asset_type` for this variant only. |

**Rules:**

- Mutually exclusive with top-level `assets:` — a spec sets exactly one of the two.
- At least one variant is required; exactly one must set `default: true`.
- Only the default variant may omit `name`. A non-default variant without one is rejected.
- Two variants with the same name (or two unnamed variants) are rejected as duplicates.
- A variant that omits `metadata`, `bin_scan`, `libc_lint` or `asset_type` inherits the spec's top-level value. A slim variant ships a different binary set than the full one, so `bin_scan: off` on a variant is an override back to unscanned, not "unset" — and `libc_lint: true` on a variant of a spec that set `libc_lint: false` turns the check back on for that variant alone.

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

The file's `binaries` list also decides what `prepare` makes executable. A tar or zip member keeps whatever mode upstream shipped it with, and some upstreams ship their interface binary non-executable at `0644` — PowerShell's `pwsh` does — so after extraction every file in the tree whose name matches a declared `binaries` entry is chmodded to `0755` if it lacks any exec bit; a mode already broader than that (`0775`, a setuid bit) is never narrowed. Nothing else is touched: an undeclared file keeps whatever the archive gave it, and a declared name that resolves to a symlink is skipped rather than chmodded — the chmod must not follow a link out of the content tree onto whatever it points at. A declared name the archive does not ship is not an error — [`bin_scan: verify`](#bin-scan) is where a missing binary is caught, on the specs that can use it. `verify` also wins over the chmod where the two overlap: a declared name found non-executable in an interface `PATH` directory fails the run with `DeclaredNotExecutable` *before* the chmod gets a chance to run, so `verify` keeps asserting what upstream actually shipped and the chmod never papers over a mismatch it was set up to catch.

The fix only reaches a name **declared** in `binaries` — whether hand-written, or filled in by [`bin_scan: auto`/`verify`](#bin-scan) from a scan. That second path has a gap of its own: the scan only reports candidates it found *already* executable, so a `0644` binary the archive ships is never picked up by an auto-fill and is therefore never in scope for the chmod either — it stays non-executable in the published bundle. A mirror hitting that case fixes it by hand-declaring the name in `binaries`, not by turning `bin_scan` on.

As with [`libc_lint`](#libc-lint), a version already published keeps the modes it was pushed with. `prepare` never re-processes a version the registry already holds, and [`pipeline patch`][cli-patch] re-references an already-published version's existing layers by digest instead of re-extracting them, so it cannot repair an exec bit either — the only way to correct a version published with the wrong mode is to delete its tags and re-mirror it.

## `bin_scan` {#bin-scan}

The `binaries` field of a package's metadata names the executables the package puts on the interface surface. Hand-listing it means keeping a list in step with whatever upstream ships in the archive; `bin_scan` derives it from the bundle instead, at mirror time.

```yaml
bin_scan: verify
metadata:
  default: metadata.json
```

| Value | Behaviour |
|-------|-----------|
| `off` (default) | Never scan. `binaries` is exactly what the metadata file declares, or absent when it declares none. |
| `auto` | Fill an absent `binaries` claim from the scan. A claim the metadata file already declares passes through unverified. |
| `verify` | Fill an absent claim exactly as `auto`, and additionally check a declared one against the extracted tree. An executable on the interface surface that the file does not list, or a listed name present but not executable, fails the run. |

The scan is not a directory walk. It reads the immediate entries of the directories reachable through the metadata's `${installPath}`-rooted `Path` environment variables whose visibility reaches the interface — so a `libexec` directory behind a `private` variable never contributes, and neither does a subdirectory of a `PATH` entry. On a non-Windows target platform a candidate counts when it carries the Unix exec bit; on a Windows target the `.exe`, `.com`, `.bat` and `.cmd` extensions are stripped and the exec bit is ignored. Symlinks are followed and claim the name of the link.

The scan only looks *below* `${installPath}`. A metadata file whose PATH variable is the bare `${installPath}` — the usual shape for an [`asset_type: binary`](#top-level) mirror, where the single executable sits at the content root — offers the scan no target directory at all, and `auto` would then publish `binaries: []`. That is not "undeclared": it is a positive claim that the package exposes nothing.

**The spec is rejected at load rather than allowed to publish that.** Enabling `bin_scan` on metadata with no `${installPath}/<dir>` interface-visible PATH entry fails with exit 65, naming the file — and, on a multi-variant spec, the variant. Every file the `metadata` block can select is checked, so a per-platform override with no scan target is caught even when the default is fine. Such a mirror either points a PATH entry at a subdirectory, or leaves `bin_scan: off` and hand-lists `binaries`; `off` with the same metadata is a perfectly good spec and keeps loading.

A file that already declares `binaries` is exempt **under `auto` only**, because `auto` passes a declared list through without scanning at all. `verify` is not exempt: it does walk the tree, and with no scan target it inspects no file and passes green whatever the archive contains — a verification that cannot fail, on a spec that says the list is checked.

Watch the per-platform files specifically, because upstream archive layouts are rarely symmetric. python-build-standalone is the worked example: Linux and macOS extract to `python/bin/`, but the Windows archive puts `python.exe` at its root with no `bin/` at all — so `metadata-windows.json` is the file that ends up with a bare `${installPath}`, on a spec whose default is fine. That is exactly the case the per-file check exists for.

All interface-visible `Path` vars are scanned, not just `PATH` — a `MANPATH` set to `${installPath}/man` is a second scan target, and the results merge into one claim. On a non-Windows target the exec bit keeps man pages out; a genuinely executable file parked under such a directory *would* be claimed.

`verify` is the mode a mirror wants once it does hand-list `binaries`: the list stops being documentation and becomes a regression test against upstream rearranging its archive. Platforms that genuinely differ get a per-platform metadata file — CMake's Windows zip has no `ccmake.exe` while its Linux and macOS archives both ship `ccmake`, so the Windows entry under [`metadata.platforms`](#metadata) declares its own shorter list.

### Interaction with `pipeline patch` {#bin-scan-patch}

[`pipeline patch`][cli-patch] never downloads anything — re-referencing published layers by digest is its entire reason to exist — so it cannot re-run a scan. Whether that matters depends on where the claim comes from:

- **The metadata file declares `binaries`** (including under `verify`, which checks the declaration rather than replacing it). Then the spec computes the whole document download-free, and `plan` and `patch` treat `binaries` like every other field: edit the list, and the next run reports drift and republishes the correction.
- **The metadata file declares none and the scan fills it.** Then the expectation cannot carry the claim at all, so the drift comparison reads the published one and adopts it before comparing. Without that, `plan` would report `metadata-drift` on every such version on every run, and each `patch` would republish the metadata with `binaries` *absent* — silently deleting a correct claim.

Adoption is deliberately limited to the second case. Applying it to a declared list would rewrite the expectation to whatever is already published, so a corrected hand-written list could never register as drift and the fix would never reach the published versions.

The consequence worth knowing: turning `bin_scan` on does **not** retroactively populate `binaries` on already-published versions, and `patch` will not do it either. A version picks up its scanned claim when it is next actually built — either by a new push, or by deleting the tag and re-mirroring it.

## `libc_lint` {#libc-lint}

A Linux binary that links against glibc cannot run on a musl-only host, and vice versa. Which one a package needs is stated in its platform key's `os.features` — `linux/amd64+libc.glibc`. `libc_lint` checks that statement against the binaries the bundle actually ships, at mirror time, and refuses to build a version whose declaration is false.

```yaml
libc_lint: false
```

| Value | Behaviour |
|-------|-----------|
| `true` (default) | Every file on the package's interface `PATH` is read. One that needs a libc family the platform key does not declare fails that version's build, naming the target, the version, the platform, the file, its dynamic loader and the platform key that would be correct. |
| `false` | The check does not run. Nothing else changes — the same bundle and the same metadata are written either way. A line naming the target and platform is logged wherever the check would have run, so the suppression is visible in the CI log. |

**Why an omitted libc is a claim, not a gap.** `os.features` matching is subset matching: an artifact whose feature list is empty demands nothing of the host, so it resolves onto *every* host. Publishing a glibc-linked tile under a bare `linux/amd64` is therefore a positive claim of libc universality, and a consumer on Alpine installs it happily and then gets `No such file or directory` naming a file that is plainly there — the kernel reporting the absent ELF interpreter. That has already happened to a published mirror.

The check reads only the ELF `PT_INTERP` header, so it costs nothing and understands nothing else: a binary needing `libstdc++` or `libicu` passes, a glibc 2.38 build declared for a glibc 2.28 host passes, and a file not on an interface `PATH` directory is never read. Statically linked binaries have no `PT_INTERP` and satisfy any declaration — which is why the four `bazelbuild` specs publishing static Go binaries under bare `linux/amd64` and `linux/arm64` keys are correct as written and pass unchanged.

**The opt-out is total.** `libc_lint: false` skips the whole check, refusals and scan-scope failures alike. That is deliberate, and the same escape hatch `ocx package create --no-libc-lint` provides: a bug in one half of the check would otherwise block every publish of a spec with no way through, and a partial bypass would leave that half still able to stop the mirror. Reach for it when the check is wrong, not when the declaration is — the fix for a real refusal is the platform key the error message hands you.

The check runs during [`pipeline prepare`](./cli.md#pipeline-prepare), between extracting the archive and compressing it — the only window in which the binaries exist on disk. [`pipeline patch`][cli-patch] never downloads anything, so it cannot run the check and never reports a libc finding; a version already published under a false declaration is corrected by fixing the platform key and re-mirroring it, not by patching metadata.

**A bundle already in the work directory is never re-checked.** `prepare` resumes from an existing `bundle.tar.xz` without extracting anything, so the check cannot run — and a bundle on disk is not evidence it ever passed: it may have been built under `libc_lint: false`, or by an ocx-mirror predating the check. Turning `libc_lint` back on therefore has no effect on any `(version, platform)` leg whose bundle the work directory already holds — a version whose `linux/amd64` leg bundled but whose `linux/arm64` leg did not is checked on arm64 and not on amd64. Delete the bundles of the legs you want re-checked (or the work directory).

## `concurrency` {#concurrency}

Tunes how much of the pipeline runs in parallel, how gently the upstream source is polled, and how a flaky push is retried. Every field has a default, so a spec that never sets `concurrency:` still gets sensible behaviour.

```yaml
concurrency:
  max_downloads: 8
  max_bundles: 4
  rate_limit_ms: 0
  max_retries: 3
  compression_threads: 0
```

**Fields:**

| Field | Type | Default | Description |
|-------|------|---------|--------------|
| `max_downloads` | integer | `8` | Maximum number of asset downloads running at once, across every `(version, platform)` task the run has in flight. |
| `max_bundles` | integer | half the host's available CPU cores, minimum 1 (`2` if the core count cannot be detected) | Maximum number of extract-and-compress tasks running at once. Bundling is CPU-bound, so the default scales with the host rather than naming a fixed number — on a 2-core runner it is 1. |
| `rate_limit_ms` | integer | `0` | Delay, in milliseconds, between paged upstream-listing requests (GitHub Releases pagination). Unrelated to push behaviour — see `max_retries` below for that. `0` means no delay. |
| `max_retries` | integer | `3` | Extra attempts a *transient* push failure is granted on top of the first — total attempts are `max_retries + 1`. `0` means a single attempt, no retry. See [Push retry](#concurrency-push-retry) below. |
| `compression_threads` | integer | `0` (auto) | Compression threads per bundle task. `0` splits the host's available cores across the `max_bundles` tasks running concurrently (at least 1 each); a positive value pins every bundle task to that many threads regardless of `max_bundles`. |

`max_pushes` is accepted and silently ignored. Push is sequential by version — see [`pipeline push`](./cli.md#pipeline-push) — so there has never been a push-parallelism knob to bound; keeping the key parsing, rather than rejecting it, is deliberate fleet compatibility for specs written before that was true.

### Push retry {#concurrency-push-retry}

A push can fail transiently — a registry connect timeout, a blip mid-upload — with no fault in the bundle itself. `pipeline push` retries exactly that class of failure, bounded by `max_retries`.

**What counts as transient.** Only an `ocx package push` exit code of **75** (`TempFail`) is retried. Exit **69** (`Unavailable`) means the failure will not change on a rerun — a registry that is down stays down — so it is never retried; neither is a registry auth rejection (exit 80) nor a child process killed by a signal (no exit code at all). Getting this 75/69 split at all needs **ocx ≥ 0.5.3** — specifically in the `ocx` binary that actually runs the push subprocess, which comes from whatever `ocx.toml`/`ocx.lock` toolchain the running `ocx-mirror` is co-located with. That is a separate pin from the `ocx` version a *generated* downstream workflow bakes into its own `setup-ocx` step (see [`ocx_mirror`](#ocx-mirror)) — the two can drift out of step, and it is the co-located one that governs retry behaviour here. An older `ocx` maps every registry-client failure to exit 69, so a transient fault on a stale pin is never retried no matter what `max_retries` says, and there is deliberately no runtime warning for this.

**The co-located `ocx` must be 0.5.5 or newer**, and that is a hard floor, not a degradation like the retry split above. From 0.5.5 the metadata sidecar no longer carries a top-level `platform` key — the platform travels on the `--platform` flag instead. Only `ocx package create` rejects a sidecar that still carries the key; `ocx package push` and `ocx package test` parse the sidecar's published form directly and simply ignore an unknown field, so the key's presence or absence makes no difference to either. An older binary reads it the other way round: it demands the recorded key and fails with `metadata sidecar has no recorded platform` and exit **65** on *every* push leg, which is not a retried code, so the run ends with nothing published. The same floor applies to the [`setup-ocx`](#ocx-mirror) pin in a generated downstream workflow — bumping the pinned `ocx-mirror` without regenerating CI leaves that repository failing every push.

**Backoff.** The first retry waits 1 second; each further attempt doubles the wait, capped at 30 seconds, with ±10% jitter layered on top of the cap — so a capped delay actually lands in the 27–33 second range, not exactly 30.

**Per-attempt timeout.** Each push attempt is bounded at 3600 seconds. This is a backstop against a wedged child process, not a throughput budget — `ocx` itself already bounds a registry request (30 seconds to connect, 120 seconds without a byte read), so a healthy upload never needs the full hour. The worst case for one tile at the default `max_retries: 3` is four attempts, close to four hours, which fits inside GitHub Actions' default 360-minute job limit — but a run pushing **two** such tiles does not. The job timeout, not this per-attempt bound, is the real outer limit on a run.

**Logging.** A retried attempt logs `push attempt {n}/{total}`; a give-up message distinguishes an exhausted retry budget from an exit code that was never eligible for retry, and both name `concurrency.max_retries` so the fix is obvious from the log line.

`pipeline patch`'s republish is bounded by the same 3600-second timeout but is deliberately never retried — it only re-emits a config blob against layers that are already published, so re-dispatching the workflow by hand is cheaper than a retry ladder.

## `cascade` {#cascade}

Whether a push re-points the rolling tags `X.Y`, `X` and `latest` at the version it just published. Defaults to `true`; `cascade: false` publishes exact version tags only, and drops the generated `cascade.yml` repair workflow along with them — a mirror with no rolling alias has no graph to repair.

The map form is the same "on", with a schedule attached to that repair workflow:

```yaml
cascade:
  schedule: "17 4 * * 1"   # optional; UTC cron, GitHub's syntax
```

A map always means enabled — `cascade: {}` is `cascade: true`. The cron string is passed through verbatim, exactly as [`versions.poll_interval`](#top-level) is: a spec is rejected (exit 65) when the expression is empty or holds a character outside cron's `0-9 A-Z a-z * / , -` charset, and GitHub validates everything beyond that. Give it a cron of its own — a `cascade.schedule` equal to `versions.poll_interval` collides in the shared concurrency group on every cycle, by construction.

**Without a schedule** (the default) `cascade.yml` is `workflow_dispatch` only, and its `dry_run` input defaults to true — a dispatch that names nothing audits.

**With a schedule** the dispatch stays, and each scheduled run repairs for real: `dry_run` has no value on a timer, so the workflow supplies `false`. A healthy scheduled run is **silent green** — the repair finds nothing, exits 0, announces nothing (the announce is never invoked with an empty tag set). Red means [exit 65][cli-pipeline-cascade] — findings the run could not re-point, the state worth a notification — or exit 1, the repair failing to run.

Green does not prove a repair ran, though: the repair step is skipped when the registry credentials are missing, and a skipped step keeps the job green. A repo whose `OCX_MIRROR_REGISTRY_TOKEN` was never set or has since been rotated therefore produces the same silent green forever. Read the run's `::notice::` once after enabling the schedule, and again after every token rotation.

The repair shares the push workflow's `concurrency` group, so neither one ever runs while the other is mid-way through re-pointing the same aliases. GitHub keeps a single *pending* run per group, so the trade is that whichever of the two is queued gets cancelled when a newer run of either workflow arrives — a scheduled repair can be dropped (grey "cancelled", never red) by a busy publish, and a pending publish can now be dropped by a repair.

Cascading interacts with [`build_timestamp`](#build-timestamp): re-pointing a rolling tag leaves the digest it used to name untagged, which is a GC hazard when `build_timestamp: none`.

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

A platform without `containers:` runs its tests natively on the runner. A platform with `containers:` runs them once per image: the generated workflow fetches a libc-matched, statically-linked `ocx` release and executes every `ocx package test` inside `docker run <image>`, so the mirrored artifact is loaded and run by that image's own libc. That is the only way an `os.features` musl or glibc claim is actually verified — an artifact that links glibc reds its Alpine leg instead of shipping a false claim. Declaring [`setup`](#container-setup) on a container narrows that claim, honestly: not "runs on stock image X", but "runs on stock image X plus these named packages" — and the packages are named right next to the image they provision.

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

### Provisioning the image (`setup`) {#container-setup}

A stock base image sometimes lacks a shared library the mirrored artifact links against — `pnpm`'s glibc build needs `libatomic.so.1`, its musl build needs `libgcc_s`. `containers[].setup` provisions the image before any test runs, per container:

```yaml
platforms:
  "linux/amd64+libc.musl":
    runner: ubuntu-latest
    containers:
      - image: "alpine:3.20"
        shell: sh
        setup:
          - apk add --no-cache libstdc++

  "linux/amd64+libc.glibc":
    runner: ubuntu-latest
    containers:
      - image: "ubuntu:24.04"
        shell: bash
        setup:
          - apt-get update
          - apt-get install -y libatomic1
      - image: "fedora:40"
        shell: bash
        setup:
          - dnf install -y libatomic
```

Each entry becomes one Dockerfile `RUN`, handed verbatim to the container's own `shell`. The image is built once per leg with `docker build` — not once per test — and every `ocx package test` invocation on that leg, across every mirrored version, reuses the resulting tag. A setup command that exits non-zero reds the leg naming the setup step, rather than surfacing as a downstream test failure that reads as an artifact defect.

**One command per entry.** `RUN` shell-form passes each entry to the container's shell unparsed, so an embedded newline would split one `RUN` into a broken Dockerfile — write the extra step as its own list entry instead.

**Deliberate scope limit.** `setup` provisions the image so the artifact can load — it is not a general pre-test hook. A leg's value is the narrow, honest claim it makes: this artifact runs on stock image X plus these named packages. Growth past a handful of lines is a visible signal the leg is doing too much (fetching fixtures, starting daemons, seeding services), and it stays visible precisely because the commands sit next to the image they provision.

Reuse across legs — the same setup needed on more than one platform — comes from YAML anchors, the same mechanism the fleet already uses for shared `assets:` and `source:` blocks; `setup:` is a plain list with nothing anchor-specific about it.

!!! note "Container legs for Python env sources (`pylock`/`pypi`)"
    The job still runs on the host runner — GitHub mounts a glibc `node` for JS actions, which Alpine's musl userland cannot execute — and only `ocx package test` is wrapped in `docker run <image>`, with the runner's CA bundle mounted so the gnu `ocx` binary can verify TLS inside a minimal image. Use an `alpine` leg to validate a `libc: musl` env end-to-end and a `debian`/`ubuntu` leg to sanity-check the glibc floor. The env under test is self-contained (local wheel layers); only its private interpreter is pulled from the registry.

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
| `containers[].id` | string | No | Stable ID used to construct JUnit filenames and GHA matrix check names. Defaults to the slugified `image` (`:` and `/` → `_`). |
| `containers[].setup` | array of strings | No | Shell commands baked into the container's image before any test runs, one per entry. See [Provisioning the image (`setup`)](#container-setup). At least one entry when the key is present — an empty list is rejected. |
| `shell` | string | No | Default shell for native legs. Defaults: `pwsh` on Windows, `bash` elsewhere. |
| `prefix` | array of strings | No | Command prefix applied before every test invocation. Defaults: `["arch", "-x86_64"]` on `darwin/amd64` with a `macos-*` runner; empty otherwise. |
| `tests` | array | No | Per-platform test override. When present, replaces the top-level `tests:` array entirely (no partial merge). |
| `min_version` | string | No | Inclusive lower bound: the first upstream version this platform applies to. See [Version applicability](#platform-version-applicability). |
| `max_version` | string | No | Exclusive upper bound: the first upstream version this platform no longer applies to. |
| `exclude` | array | No | Individual `(version[, range])` holes within the window. See [Version applicability](#platform-version-applicability). |

**Platform key validation:**

- Must parse as a platform key: `<os>/<arch>[/<variant>][+libc.<flavor>[,...]]` — the same grammar [`assets`](#assets-libc) uses. Quote any key containing `+`.
- A key declaring a libc must be tested under that libc: every image on `linux/amd64+libc.musl` has to be a musl base (Alpine), and every image on a `+libc.glibc` key a glibc base. The mismatch is rejected at generate time with exit 65 — a musl-static binary runs fine under glibc, so an Alpine claim tested in Ubuntu goes green having verified nothing.
- `containers[].setup`, when present, must declare at least one command — an empty list is rejected (exit 65: drop the key instead).
- Every `setup` entry must be non-blank and a single line. Each entry becomes one Dockerfile `RUN`; a blank entry or one containing a newline is rejected (exit 65) rather than emitted as a broken Dockerfile.
- No `setup` entry may end in a backslash. A trailing `\` is a line continuation that would absorb the following `RUN` as its own arguments — the build exits 0 with that layer never applied — so it is rejected (exit 65).
- A key other than `runner`, `containers`, `prefix`, `shell`, `tests`, `min_version`, `max_version`, or `exclude` under `platforms.<p>` — and a key other than `image`, `shell`, `id`, or `setup` under `containers[]` — is rejected at parse time (`deny_unknown_fields`, exit 65), not silently dropped. This is what makes a `setup:` written one level too high, on the platform instead of the container, a loud error.

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
    Generated jobs install the toolchain via the [`ocx-sh/setup-ocx`][setup-ocx] action, which activates the mirror repository's project toolchain (`ocx.toml` / `ocx.lock`) onto `PATH` — `ocx-mirror` and `ocx` both come from there. Every generated job pins one `ocx` version end to end: `setup-ocx` is called with an explicit `version:` input, and container test legs download the statically-linked release of that same version. The version is a constant in the renderer, not a spec field, so the whole fleet tests against one binary and it advances when the repository's pinned `ocx-mirror` does.

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
| `schedule` | string | No | UTC cron putting the generated `announce-from-registry.yml` catch-up workflow on a timer. Absent → that workflow is dispatch-only. See **Catching up an existing mirror** below. |

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

Every mirror with an `announce:` block gets a second generated workflow, `announce-from-registry.yml`, for exactly this. It lists every tag the target repository currently holds, then unions them onto the committed index entry. It is never triggered by a push. Dispatch it from the repository's **Actions** tab, or:

```sh
gh workflow run announce-from-registry.yml --repo <owner>/<mirror> -f dry_run=false
```

`dry_run` defaults to **true**: the run reports whether the index would change (`updated` or `unchanged`) and discards the rebuilt entry without opening a pull request. Pass `dry_run=false` to open it for real.

**On a timer.** `announce.schedule` adds a `schedule:` trigger to that workflow, keeping the dispatch:

```yaml
announce:
  package: bazelbuild/bazelisk
  fork: ocx-contrib/index
  schedule: "23 5 * * 2"   # optional; UTC cron, GitHub's syntax
```

The cron string is passed through verbatim, exactly as [`cascade.schedule`](#cascade) is: a spec is rejected (exit 65) when the expression is empty or holds a character outside cron's `0-9 A-Z a-z * / , -` charset, and GitHub validates everything beyond that.

`dry_run` has no value outside a dispatch, so the workflow resolves it itself — `false` on a schedule event, the input's value on a dispatch. **A scheduled run therefore announces for real.** A run that finds nothing new is silent: an unchanged announce commits nothing, and opens no pull request unless an earlier run left unmerged commits on the announce branch, which it then ensures a pull request for (`unchanged` *with* a pull request URL in [`pipeline announce`][cli-announce]'s log). A caught-up mirror with nothing stranded produces one green run per cycle and no index traffic.

Green is not proof an announce ran, though: on a target other than `ghcr.io` — whose credential probe is constant — the announce step is skipped when the registry credentials are missing, and a skipped step keeps the job green. A repo whose `OCX_MIRROR_REGISTRY_TOKEN` was never set or has since been rotated therefore produces the same silent green forever. Read the run's `::notice::` once after enabling the schedule, and again after every token rotation.

The workflow keeps a `concurrency` group of its own rather than joining the push workflow's the way [`cascade.yml`](#cascade) does: it writes index pull requests only, never registry tags, so concurrent announce writers contend on a per-package index branch rather than on tags — the fast-forward path is compare-and-swap with a retry, and the spent-branch reset path can drop a racing branch commit, which the next full from-registry run re-adds. Joining the publish group would instead let a queued push cancel the pending catch-up. Give `announce.schedule` a cron of its own all the same: sharing one with [`versions.poll_interval`](#top-level) schedules the catch-up against the push job's own closing announce.

The catch-up is **additive**, on the same footing as the push job's `--tags-from-file`: it cannot drop a tag the index already commits, and yank markers survive. Running it against a mirror that is already current is a no-op, so it is safe to dispatch on suspicion.

Its `ocx-mirror` entry point is [`pipeline announce`][cli-announce]; the same command runs locally against a checkout.

(`--refresh` on `ocx package announce` solves a different problem — it re-observes the tags already committed, picking up a digest that moved, and never adds one.)

**Validation:**

- `package` must be a `<namespace>/<package>` pair of lowercase alphanumerics with `.`, `_` or `-`. A bare tool name is rejected with exit code 65 (`DataError`).
- `fork` and `index_repo` must each be an `<owner>/<repo>` pair. A pasted URL is rejected the same way.
- `schedule`, when present, must be non-empty and hold only cron's `0-9 A-Z a-z * / , -` charset. Anything else is rejected before a workflow is written, on the same reasoning as [`cascade.schedule`](#cascade).

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
├── patch.yml
├── cascade.yml
├── announce-from-registry.yml
├── mirror-buildifier.yml
├── describe-buildifier.yml
├── patch-buildifier.yml
├── cascade-buildifier.yml
├── announce-from-registry-buildifier.yml
├── mirror-buildozer.yml
├── describe-buildozer.yml
├── patch-buildozer.yml
├── cascade-buildozer.yml
├── announce-from-registry-buildozer.yml
├── mirror-unused-deps.yml
├── describe-unused-deps.yml
├── patch-unused-deps.yml
├── cascade-unused-deps.yml
├── announce-from-registry-unused-deps.yml
└── verify-generated.yml                  # one guard, names all four specs
```

Naming a nested spec file `mirror.yml` is convention, not a requirement — the generated filenames derive from the spec's **directory**, never its filename (below). Keep the filename anyway: it matches every other spec in the repository, and it is the directory — not the name — that `--repo-root`'s default and the collision check both reason about.

**Generated file names.** A spec at the repository root keeps today's filenames byte for byte — `mirror.yml`, `describe.yml`, `patch.yml`, `cascade.yml`, `announce-from-registry.yml` — so a repository that adds its first nested spec never has to touch the workflows it already published. A spec anywhere else gets every filename suffixed with its directory, `/` joined by `-`:

| Spec path (relative to repo root) | Suffix | `mirror.yml` becomes |
|---|---|---|
| `mirror.yml` | *(none)* | `mirror.yml` |
| `buildifier/mirror.yml` | `-buildifier` | `mirror-buildifier.yml` |
| `a/b/mirror.yml` | `-a-b` | `mirror-a-b.yml` |

Because the suffix comes from the directory alone, **a directory may hold only one spec** — two specs sharing a directory, whatever their filenames, would render the same workflow set and silently overwrite each other. `generate ci` rejects this with exit 64 before writing anything.

Every generated pipeline invocation in a nested spec's workflows names its own spec explicitly — `pipeline plan --spec buildifier/mirror.yml`, and likewise for `prepare`, `push`, `describe`, `announce`, `patch`, `cascade`. The root spec's invocations never carry `--spec`: its path is exactly what every subcommand already defaults to, which is what keeps the root workflows byte-identical.

**`--repo-root`.** Generated files are written under `--repo-root`, and every filename above is computed relative to it. Left unset, it defaults to the deepest directory every `--spec` given shares — for a single spec that is simply its parent directory, so `generate ci --spec /elsewhere/repo/mirror.yml` still writes into that repository rather than the current directory. A spec that does not resolve under `--repo-root` (explicit or inferred) is rejected with exit 64, naming `--repo-root` as the fix.

**CI triggers per spec.** The root spec's workflow keeps the repository-wide trigger list it has always had (its own spec file, `scripts/**`, `tests/**`, `metadata*.json`) plus its own workflow file. A nested spec's workflow instead triggers only on its own subtree — `buildifier/**` plus `.github/workflows/mirror-buildifier.yml` — never the repository-wide list, so editing `buildozer/` never wakes `buildifier`'s workflow. The generated `describe-<dir>.yml` follows the same rule for its own triggers (`CATALOG.md` / `logo.*` at the root, `<dir>/**` when nested); `patch-<dir>.yml`, `cascade-<dir>.yml` and `announce-from-registry-<dir>.yml` have no path triggers at all — they are dispatched, or run on a timer when their spec opts in ([`cascade.schedule`](#cascade), [`announce.schedule`](#announce)). Each carries a distinct `name:` — sibling `describe` workflows sharing a name would share a `concurrency.group` too, since it keys on `github.workflow`.

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
[cli-pipeline-cascade]: ./cli.md#pipeline-cascade
