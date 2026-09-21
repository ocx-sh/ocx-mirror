# plan.json

`plan.json` is the forge-neutral contract `ocx-mirror package pipeline plan`
writes: the versions that need work, the assets already resolved for them, and
the CI legs that would test them. This page documents its shape and its
compatibility promise.

It is a **supported output**, not an implementation detail. The GitHub workflow
[`pipeline generate ci`](./cli.md#pipeline-generate-ci) renders is one consumer
of this document; a GitLab pipeline, a Jenkins job, or a shell script on a cron
box is an equally valid one, and everything such a renderer needs is in here.

## Who writes it, who reads it {#producers-consumers}

**Producer.** [`ocx-mirror package pipeline plan --format json`](./cli.md#pipeline-plan).
JSON is selected automatically when `GITHUB_ACTIONS=true`, so the generated
workflow's `discover` job writes the file without passing the flag. The command
is side-effect-free apart from the derived PEP 751 locks a `source.type: pypi`
mirror needs.

**Consumers.**

- [`ocx-mirror package pipeline prepare --plan <FILE>`](./cli.md#pipeline-prepare)
  — hands `prepare` the assets discover already resolved, so the source is
  crawled once per run instead of once per version.
- Your own renderer. Everything below is written for that reader.

**What it deliberately does not carry.** Nothing in this document names a CI
system. `runner` is a label set, not a GitHub concept; there is no `secrets.`
context, no workflow path, no cron expression, no job name. A field that could
only mean something on one forge does not belong here.

## Document shape {#shape}

An archive mirror with one container platform, one native platform, a
per-platform test override and a `setup:` block:

```json
{
  "schema_version": 4,
  "has_new": true,
  "has_drift": false,
  "versions": [
    {
      "version": "3.29.0_20260610",
      "source_version": "3.29.0",
      "platforms": ["linux/amd64", "darwin/arm64"],
      "kind": "new",
      "variant": null,
      "assets": [
        {
          "platform": "linux/amd64",
          "asset_name": "cmake-3.29.0-linux-x86_64.tar.gz",
          "url": "https://github.com/Kitware/CMake/releases/download/v3.29.0/cmake-3.29.0-linux-x86_64.tar.gz",
          "digest": "sha256:1b2c3d…"
        },
        {
          "platform": "darwin/arm64",
          "asset_name": "cmake-3.29.0-macos-universal.tar.gz",
          "url": "https://github.com/Kitware/CMake/releases/download/v3.29.0/cmake-3.29.0-macos-universal.tar.gz"
        }
      ]
    }
  ],
  "target": "ocx.sh/cmake",
  "ocx_mirror_rev": null,
  "legs": {
    "linux/amd64": {
      "runner": ["ubuntu-latest"],
      "platform_slug": "linux_amd64",
      "docker_platform": "linux/amd64",
      "containers": [
        {
          "id": "alpine_3_20",
          "image": "alpine:3.20",
          "shell": "sh",
          "libc": "musl",
          "setup": ["apk add --no-cache libstdc++"]
        }
      ],
      "tests": [{ "name": "version", "command": "cmake --version" }]
    },
    "darwin/arm64": {
      "runner": ["macos-latest"],
      "platform_slug": "darwin_arm64",
      "docker_platform": "darwin/arm64",
      "containers": [{ "id": "_native_", "shell": "bash" }],
      "tests": [{ "name": "smoke", "script": "tests/smoke.star" }]
    }
  },
  "versions_resolved": { "min": "3.20.0", "min_inclusive": true, "max_inclusive": false }
}
```

Generate the machine-readable schema for this document with
[`ocx-mirror schema plan`](./cli.md#schema). Its `$id` is
`https://ocx.sh/schemas/plan/v4.json`.

## Fields {#fields}

### Top level

| Field | Type | Since | Meaning |
|---|---|---|---|
| `schema_version` | integer | 1 | Wire version of this document. See [Compatibility](#compatibility). |
| `has_new` | boolean | 1 | At least one entry is `new` or `backfill-partial` — something must be downloaded and built. Deliberately **false** when the only findings are drift. |
| `has_drift` | boolean | 3 | At least one published version's metadata no longer matches the spec. Report-only; acted on by [`pipeline patch`](./cli.md#pipeline-patch). |
| `versions` | array | 1 | Versions needing action, oldest first. May be empty. |
| `target` | string | 1 | Full OCI repository (`registry/repository`). |
| `ocx_mirror_rev` | string \| null | 1 | The `ocx_mirror.rev` the spec declares. `null` when it declares none — which is most specs; it is a hand-maintained traceability field, not a derived one. |
| `legs` | object | 4 | The spec's CI matrix, keyed by platform key. Empty object when the spec declares no `platforms:`. |
| `versions_resolved` | object | 4 | The version window this run actually filtered by, with every [`versions:`](./mirror-yml.md#versions) bound resolved. Shape: `{min?, min_inclusive, max?, max_inclusive}`. |

### `versions[]`

| Field | Type | Since | Meaning |
|---|---|---|---|
| `version` | string | 1 | The tag the pipeline will publish — build-stamped, and variant-prefixed for a non-default archive variant (`slim-3.29.0_20260610`). Every downstream step keys on this string: it is the **fan-out key**, and what [`prepare --version`](./cli.md#pipeline-prepare) takes. |
| `platforms` | [string] | 1 | Base `os/arch` keys still needing work for this version. Env sources dedupe their `+libc.*` wheels keys onto the base here; the full keys are in `assets[].platform`. |
| `kind` | `"new"` \| `"backfill-partial"` \| `"metadata-drift"` | 1 (`metadata-drift`: 3) | Why the version is listed. |
| `source_version` | string | 2 | The upstream version before normalisation (`3.29.0` for `3.29.0_20260610`). Informational — `prepare --version` accepts it too, but a variant spec stamps two tags from one release, so the bare form names both and is refused as ambiguous. Fan out on `version`. |
| `variant` | string \| null | 2 | Archive variant this entry belongs to; `null` for the default. |
| `assets` | array | 2 | Resolved download targets — see below. Empty for a `metadata-drift` entry. |
| `pylock` | string | 2 | Derived PEP 751 lock path, **relative to this file's own directory**. Present only for `source.type: pypi`; the key is omitted otherwise. |

### `versions[].assets[]`

| Field | Type | Meaning |
|---|---|---|
| `platform` | string | Full platform key, including any `+libc.*` suffix (env sources carry the wheels key here). |
| `asset_name` | string | Upstream file name — drives archive-type detection. |
| `url` | string (URL) | Direct download URL, resolved by discover's single source crawl. |
| `digest` | string | Publisher-declared `sha256:<hex>`. Absent when the source declared none. Defined by the sibling `verify:` work — see [`verify`](./mirror-yml.md#verify) for when it is checked. |

### `legs.<platform key>`

The key is the [`platforms:`](./mirror-yml.md#platforms) key verbatim,
including any `+libc.*` suffix.

| Field | Type | Meaning |
|---|---|---|
| `runner` | [string] | Runner label set: the job must run on a runner carrying **all** of these. Always a list, even where the spec wrote one label. GitHub maps it to `runs-on`, GitLab to `tags`. |
| `platform_slug` | string | The artifact/JUnit join key (`linux/amd64+libc.musl` → `linux_amd64_libc.musl`). Use it verbatim. |
| `docker_platform` | string | The key with any `+libc.*` suffix stripped — what `docker run --platform` accepts. |
| `containers` | array | One entry per container image, or exactly one `_native_` entry when the platform declares no `containers:`. Never empty. |
| `tests` | array | The effective test list **for this platform**: `platforms.<key>.tests` when the spec sets it, otherwise the top-level `tests:`. Never a merge — a per-platform list replaces the global one outright, so read it per leg and never from a global field. Never empty. |

### `legs.<key>.containers[]`

| Field | Type | Meaning |
|---|---|---|
| `id` | string | The JUnit / gating key. `_native_` for a platform with no containers. `pipeline push` looks results up by this exact string and fails closed when it finds none — do not invent your own. |
| `image` | string | OCI image to run the tests in. **Absent** on the `_native_` leg. |
| `shell` | string | `bash` \| `sh` \| `pwsh` — resolved, never left to the reader to infer. |
| `libc` | `"gnu"` \| `"musl"` | The image's libc family, which selects the statically-linked `ocx` to mount. **Absent** on the `_native_` leg. |
| `setup` | [string] | Provisioning commands, one per image build step, run once per leg before any test. Absent when the container declares none. |

### `legs.<key>.tests[]`

Exactly one of the three payload fields is set.

| Field | Type | Meaning |
|---|---|---|
| `name` | string | Unique per spec, `^[a-zA-Z][a-zA-Z0-9_-]*$`. Becomes the JUnit testcase name — `pipeline push` gates on it. |
| `command` | string | Shell command, run verbatim in the leg's `shell`. |
| `script` | string | Path to a Starlark script, **relative to the repository root** (not to the spec's directory). Already checked to exist at validation time. |
| `script_inline` | string | Starlark source, verbatim. |

### `versions_resolved`

| Field | Type | Meaning |
|---|---|---|
| `min` | string | Resolved lower bound. Absent when the spec sets none. |
| `min_inclusive` | boolean | Whether `min` itself is in the window. `true` for the shorthand spelling. |
| `max` | string | Resolved upper bound. Absent when the spec sets none. |
| `max_inclusive` | boolean | Whether `max` itself is in the window. `false` for the shorthand spelling. |

Defined by the sibling `versions:` work — see [`versions`](./mirror-yml.md#versions)
for the bound forms a spec may write.

## Rendering your own pipeline {#rendering}

**The fan-out is `versions[] × legs`.** One prepare job per `versions[]` entry,
one test job per `(version, platform key, container)` triple where the platform
key appears in that version's `platforms`.

**Pass `versions[].version`, never `source_version`.** It is the string
`prepare --version`, `patch --version` and the JUnit file names all key on. The
adjacent `source_version` is there to say what upstream called the release, and
passing it fans out correctly only while the spec declares no `variants:`.

Drop every entry whose `kind` is `metadata-drift` before building the prepare
matrix: a drift entry carries no assets by construction, so a prepare leg for
one aborts looking for a bundle nothing wrote. Gate the whole download-and-build
chain on `has_new`, which already excludes them.

The reference GitHub renderer projects the plan with exactly two `jq` lines:

```sh
echo "has_new=$(jq -r '.has_new' plan.json)" >> "${GITHUB_OUTPUT}"
echo "versions=$(jq -c '[.versions[] | select(.kind != "metadata-drift") | {version, platforms, kind}]' plan.json)" >> "${GITHUB_OUTPUT}"
```

The full document — asset URLs included — then travels to the prepare legs as an
artifact, which is what keeps the source crawled once per run.

**The JUnit contract.** [`ocx-mirror package pipeline push`](./cli.md#pipeline-push)
keys its verdict on `(version, platform_slug, container id, test name)` and
**fails closed on a missing result**. Write one file per leg at
`junit-{version}-{platform_slug}-{id}.xml` with classname
`{version}.{platform_slug}.{id}`, using the `platform_slug` and
`containers[].id` this document gives you. Do not re-derive either: a renderer
that slugifies the platform key itself gets `linux_amd64_libc.musl` wrong, and
push then finds no result for a leg that actually ran green.

**Running the three test kinds.** Each entry in `legs.<key>.tests[]` sets
exactly one payload field, and all three run in the leg's `shell`:

| Field set | How to run it |
|---|---|
| `command` | Execute the string verbatim in `shell`. |
| `script` | Repository-root-relative path — hand it to `ocx package test --script <path>`. |
| `script_inline` | Pipe the source to `ocx package test --script -`. |

On a container leg, wrap each invocation in
`docker run --platform <docker_platform> <image>` with a `libc`-matched static
`ocx` mounted in; on a `_native_` leg, call `ocx package test` on the runner
directly.

## Compatibility {#compatibility}

> `schema_version` increments on **every** change to this document's shape,
> additive or not. It is the only field whose meaning is frozen.
>
> Within one major line of `ocx-mirror`, changes are additive: new fields
> appear, existing fields keep their name, type and meaning.
>
> **Consumers must:**
>
> - **Ignore unknown fields.** New keys appear at any level and are not a
>   version bump you need to react to.
> - **Not hard-fail on a higher `schema_version`.** Read the fields you know.
>   A document from a newer `ocx-mirror` is still readable for everything that
>   existed when you wrote your renderer.
> - **Treat an absent optional key as absent**, not as an error — `pylock`,
>   `digest`, `image`, `libc` and `setup` are omitted rather than nulled.
>
> **ocx-mirror will:**
>
> - Bump `schema_version` and note it in the [changelog](../changelog.md)
>   before removing a field, renaming one, or changing what one means.
> - Never reuse a field name for a different meaning.
> - Keep `pipeline prepare --plan` able to read the plan documents its own
>   major line produced.
>
> A removal or a meaning change is a breaking change and gets a release note.
> Everything else does not.

## Changelog {#history}

| Version | Change |
|---|---|
| 1 | Initial document: `schema_version`, `has_new`, `versions[]`, `target`, `ocx_mirror_rev`. |
| 2 | `source_version`, `variant`, resolved `assets[]` and `pylock` per version entry — so `prepare --plan` consumes the discover crawl instead of re-crawling. |
| 3 | The `metadata-drift` kind and the `has_drift` gate. |
| 4 | `legs`, `versions_resolved`, and a per-asset `digest`. |
