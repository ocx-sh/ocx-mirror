# mirror.yml Reference

`mirror.yml` describes one tool to mirror — where to fetch upstream releases, which platforms to build for, how to test each bundle, and how to report results. The file is consumed by `ocx-mirror package sync`, `ocx-mirror package check`, and all `ocx-mirror package pipeline` subcommands.

## Top-level keys {#top-level}

| Key | Type | Required | Purpose |
|-----|------|----------|---------|
| `name` | string | Yes | Tool name, used in log output and notify messages |
| `target` | object | Yes | OCI registry and repository to push to |
| `source` | object | Yes | Upstream release source: [GitHub Releases][github-releases], URL index, a committed [PEP 751 `pylock.toml`](#pylock), or an [index-discovered PyPI package](#pypi-source) |
| `assets` | object | Yes* | Platform → regex list mapping for selecting upstream release archives. Not used by `source.type: pylock`/`pypi`. |
| `asset_type` | string | No | `Archive` (default) or `Binary`. Not used by `source.type: pylock`/`pypi`. |
| `python` | object | No* | Interpreter version/ABI + `interpreter_package`, plus optional [`lock`](#python-lock) and [`entrypoints`](#entrypoints) config. **Required** for `source.type: pylock` or `pypi`. See [Python apps](#pylock). |
| `variants` | array | No* | Wheel-selection variants (libc, manylinux floor, [`wheel_priority`](#wheel-priority) ranking). Used by `source.type: pylock`/`pypi`. See [Python apps](#pylock). |
| `wheel_scope` | string | No | Repo-naming scope prefix for [shared wheel layers](#shared-wheel-layers) (`source.type: pylock`/`pypi`). Default `pip-packages`. |
| `build_timestamp` | string | No | Per-build tag suffix: `datetime` (default), `date`, or `none`. See [build_timestamp & GC-safe publishing](#build-timestamp). |
| `cascade` | boolean | No | Cascade rolling tags on push (`true` by default). See [build_timestamp & GC-safe publishing](#build-timestamp). |
| `versions` | object | No | Version filter (min/max bounds, `new_per_run`, backfill order) |
| `verify` | object | No | Checksum verification options |
| `concurrency` | object | No | Parallel download and push limits |
| `tests` | array | No* | Commands to run against each installed bundle. Required when `pipeline generate ci` is used. |
| `platforms` | object | No* | GHA runner and container matrix. Required when `pipeline generate ci` is used. |
| `ocx_mirror` | object | No* | ocx-mirror version pin for generated workflows. Required when any Linux platform declares containers. |
| `notify` | object | No | Discord webhook notification settings |

The `tests`, `platforms`, `ocx_mirror`, and `notify` keys are used only by `ocx-mirror package pipeline` subcommands. `sync` and `check` ignore them.

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

When a tool ships separate builds for different C libraries on the same `os/arch` (e.g. glibc and musl on `linux/amd64`), append a `+libc.<flavor>` tag to the key. The tag is published into the OCI image index as an `os.features` entry, so a client (`ocx install`) selects the build matching its host libc:

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

Everything downstream of "a lock is in hand" — wheel selection ([`variants`](#variants)), entrypoint synthesis ([`python.entrypoints`](#entrypoints)), composition, and [shared wheel layers](#shared-wheel-layers) — is identical for both.

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
  interpreter_package: "ocx.sh/cpython:3.14.6"   # OCX package providing python3
variants:
  - default: true                 # the unnamed default variant → bare tags
    libc: gnu                     # gnu | musl
    min_manylinux: "2_28"         # manylinux floor for compiled wheels
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

Each derived lock is written under `--locks-dir` (a `pipeline plan` flag, default `./locks`, relative to the command's working directory — the same directory `plan.json` is written to) as `<package>-<version>.pylock.toml`, with a relaxed `requires-python` floor (works around a known `uv` over-strict-patch-pin issue) and a provenance comment header. `pipeline prepare --plan` reads the path straight from the plan instead of re-deriving; a standalone `pipeline prepare` (no `--plan`) re-derives it from scratch.

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

### `variants`

Each variant is a wheel-selection axis. The **default** variant (`default: true`, unnamed) publishes to bare tags. `libc` (`gnu`/`musl`), `min_manylinux`, and `min_musllinux` gate which compiled Linux wheels are eligible. For a pure `py3-none-any` app the variant does not change wheel selection (the pure wheel matches any target).

A variant may also carry its own `interpreter_package`, overriding the top-level `python.interpreter_package` for that variant's env. This is how a `libc: musl` variant depends on a **musl-libc CPython** while the default variant keeps the glibc one:

```yaml
python:
  interpreter_package: "ocx.sh/cpython:3.14.6"          # glibc default
variants:
  - default: true
    libc: musl
    min_musllinux: "1_2"
    interpreter_package: "dev.ocx.sh/ocx/cpython-musl:3.14.6"   # musl override
```

The musl interpreter lives in a **separate repository** (`…/cpython-musl`), not a musl candidate inside the glibc `cpython` index: OCX index candidates are keyed by `os/arch` only, so musl and glibc `linux/amd64` cannot coexist under one tag — libc is the variant axis. Publish one by mirroring a python-build-standalone `…-unknown-linux-musl-install_only.tar.gz` as a single-layer archive (`strip=1`, `PATH=${installPath}/bin`). Pair a `libc: musl` variant with an [`alpine` container test leg](#platforms) to validate it end-to-end.

#### `variants[].wheel_priority` {#wheel-priority}

An ordered list of PEP 425 platform-tag *prefixes*, ranking which of a package's tag-compatible wheels wins when more than one applies. Earlier entries in the list outrank later ones; a tag matching none of the list ranks lowest (today's tag-priority-only ordering, unchanged). Absent (the default) or empty: every wheel ranks identically and the existing `uv-platform-tags` priority alone decides — no behavior change.

```yaml
variants:
  - default: true
    wheel_priority: ["any"]   # prefer pure-Python wheels over compiled ones
```

**`["any"]`-first is mandatory for fully-static interpreters.** A statically-linked interpreter build cannot `dlopen` a compiled C-extension wheel. Without `wheel_priority: ["any"]`, normal tag priority ranks an exact compiled wheel (e.g. `manylinux_2_28_x86_64`) above a pure `py3-none-any` one — the wrong choice for such a build. Listing `"any"` first flips that ordering so the pure wheel wins whenever both are candidates.

**Ranking is a reorder, not a re-admission.** `wheel_priority` only reorders wheels that already passed tag-compatibility against the `libc`/`min_manylinux`/`min_musllinux` floor above — it can never select a wheel those constraints already excluded. Ranking `musllinux` first on a `gnu`/manylinux target still cannot select a musllinux-only wheel; there is simply no compatible candidate to rank.

### What is published

Each app version becomes an environment package: one content-addressed `tar.zst` layer per wheel (deterministic repack — see the [conventions ADR](https://github.com/ocx-sh/ocx-mirror/blob/main/.claude/artifacts/adr_ocx_python_conventions.md)), a composed `metadata.json` (private interpreter dependency, `PYTHONPATH`/`PATH` env, and a synthesized entrypoint per `[console_scripts]` entry), and an always-present **`python3` entrypoint** so even a *library* env (no console script of its own — e.g. `google-cloud-aiplatform`) is runnable and importable: `ocx run <env> -- python3 -c "import pkg"`.

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
    command: bash ./tests/smoke.sh
```

**Rules:**

- Required: must contain at least one entry when used with `pipeline generate ci`.
- `name` must be unique within the file and must match `^[a-zA-Z][a-zA-Z0-9_-]*$`. The name appears as the JUnit test-case name, so it must be stable across runs.
- `command` is a single-line string. Multi-line scripts must be files in the mirror repository and invoked via shell (`bash ./tests/smoke.sh`, `pwsh -File ./tests/smoke.ps1`).
- No `script` field or auto-detection — command-only by design.

**Environment exposed to every test command:**

| Variable | Value |
|----------|-------|
| `OCX_INSTALL_DIR` | Path where `ocx package test` materialized the package |
| `OCX_VERSION` | Mirrored version string (e.g., `3.29.0`) |
| `OCX_PLATFORM` | Platform slug (e.g., `linux/amd64`) |
| `OCX_IMAGE` | Container image; empty on native legs |
| `OCX_TEST_NAME` | The `tests[].name` value for this invocation |

## `platforms` {#platforms}

Declares the GHA runner and container matrix for the generated workflow. Each key is a platform slug in `<os>/<arch>` form.

!!! note "Container test legs"
    A platform with `containers:` runs its tests inside each listed image (Linux only) instead of on the runner directly. The job still runs on the host runner — GitHub mounts a glibc `node` for JS actions, which Alpine's musl userland cannot execute — and only `ocx package test` is wrapped in `docker run <image>` with a statically-linked, **libc-matched** `ocx` release binary mounted in (musl for `alpine*` images, gnu otherwise). The runner's CA bundle is mounted so the gnu `ocx` can verify TLS in a minimal image. Use an `alpine` leg to validate a `libc: musl` env end-to-end and a `debian` leg to sanity-check the glibc floor. The env under test is self-contained (local wheel layers); only its private interpreter is pulled from the registry.

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

- Must match `^[a-z0-9_-]+/[a-z0-9_-]+$`.

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

Pins the `ocx-mirror` version used in generated workflow jobs (`discover`, `prepare`, `push`, `notify`).

```yaml
ocx_mirror:
  release_tag: v0.7.2
  rev: abc123def0123456789012345678901234567890
```

**Fields:**

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `release_tag` | string | Yes (when any Linux platform has containers) | ocx-mirror release tag. Used for musl-static artifact download on Linux container legs. Must match `^v\d+\.\d+\.\d+(-[a-z0-9.]+)?$`. |
| `rev` | string | No | Full 40-character git SHA. When set, takes precedence over `release_tag` for `cargo install` paths. When both present, `release_tag` is still used for musl artifact download. Must match `^[0-9a-f]{40}$`. |

When all Linux platforms are container-less (native-only mirror), `release_tag` is optional and `rev` alone is sufficient.

!!! info "How ocx-mirror is installed in CI"
    Generated jobs install the toolchain via the [`ocx-sh/setup-ocx`][setup-ocx] action, which activates the mirror repository's project toolchain (`ocx.toml`) onto `PATH` — `ocx-mirror` and `ocx` both come from there.

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

## Spec inheritance {#inheritance}

`mirror.yml` files support an `extends:` key for shallow merge from a parent spec. Child keys override parent keys at the top level. This is useful for sharing `source` and `assets` across variants of the same tool.

```yaml
extends: ./base-cmake.yml
target:
  registry: private.registry.example.com
  repository: internal/cmake
```

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

ocx_mirror:
  release_tag: v0.7.2

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

<!-- commands -->
[cmd-pipeline]: ./cli.md#pipeline
[cmd-sync]: ./cli.md#sync
