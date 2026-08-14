# registry.yml Reference

`registry.yml` describes a **whole-registry mirror**: it copies the OCI content of one or more upstream OCX indexes into a registry you control, and writes an index tree pointing at your copy. A machine configured against that tree resolves and installs every mirrored package without reaching the public internet.

It is consumed by `ocx-mirror registry sync`. One repository holds exactly one `registry.yml` — unlike [`mirror.yml`](mirror-yml.md), where a repository may hold many specs for many tools.

The two files describe different jobs. `mirror.yml` packages an upstream tool's release archives into an OCX package you publish. `registry.yml` copies packages someone else already published.

## Top-level keys {#top-level}

| Key | Type | Required | Purpose |
|-----|------|----------|---------|
| `kind` | string | Yes | Must be `registry`. Distinguishes this file from a `mirror.yml`. See [`kind`](#kind). |
| `target` | object | Yes | Destination registry and the repository **prefix** everything is written beneath. See [`target`](#target). |
| `output` | path | Yes | Directory the index tree is written into — one subtree per source. See [`output`](#output). |
| `destination` | string | Yes | Template deciding each package's destination repository. See [`destination`](#destination). |
| `sources` | array | Yes | Upstream indexes to mirror, at least one. See [`sources`](#sources). |
| `on_error` | string | No | `continue` (default) or `fail_fast`. See [`on_error`](#on-error). |
| `concurrency` | object | No | Blob copy limits and retry count. See [`concurrency`](#concurrency). |

`extends:` works the same way it does in `mirror.yml` — a base file is shallow-merged and the child's keys win.

## `kind` {#kind}

```yaml
kind: registry
```

Mandatory, and the value must be exactly `registry`. It exists so that pointing a command at the wrong file gives you a sentence you can act on. Without it, feeding a `registry.yml` to a command expecting a `mirror.yml` reports `unknown field 'sources'` — technically true, and useless. The discriminator is read before anything else, so the error names the actual problem.

## `target` {#target}

```yaml
target:
  registry: artifactory.corp.example
  repository: ocx-mirror
```

`repository` here is a **prefix**, not a full repository path. Every package the mirror copies lands beneath it, at `<repository>/<expanded destination>`. In an Artifactory deployment this is your repo-key.

Both values are checked against the OCI naming grammar when the file loads, so a typo fails immediately rather than during a push hours later.

!!! warning "Artifactory must be an OCI-type repository"

    Create the destination as an **OCI** repository (Artifactory 7.74+), not a legacy Docker-type repo-key. A Docker-type repo-key accepts the login and then fails every upload with `unable to upload blob … unknown: Not Found`. Choosing OCI now avoids a repository migration later. Referrers support needs 7.90.1+.

## `output` {#output}

```yaml
output: ./public
```

A **parent directory**, not a template. Each source gets its own subtree beneath it, named by that source's [`as`](#sources) value:

```
public/
└── ocx.sh/
    ├── config.json
    ├── c/index.json
    └── p/<namespace>/<package>.json
```

Serve `public/` with any static file server and point consumers at `https://<host>/ocx.sh`. Nothing but index content is ever written here — no lock files, no caches, no state. The tree is safe to commit to git and to rsync.

The state a run *does* need — the source-catalog digest behind the no-op short-circuit below, and the index lock files — lives outside `output:` entirely, under `--cache-dir` (default `${XDG_CACHE_HOME:-~/.cache}/ocx-mirror`).

A run that changes nothing rewrites nothing, leaving file contents *and* timestamps untouched, which is what makes a scheduled sync against a committed tree quiet rather than a source of empty commits.

## `sources` {#sources}

One entry per upstream index. At least one is required.

```yaml
sources:
  - registry: ocx.sh
    index: https://index.ocx.sh
    as: ocx.sh
    include: ["kubernetes/*", "hashicorp/*"]
    exclude: ["*/internal-*"]
    trusted_hosts: []
```

| Key | Type | Required | Purpose |
|-----|------|----------|---------|
| `registry` | string | Yes | The logical registry name this index publishes |
| `index` | URL | Yes | Where that index tree is served |
| `as` | string | No | Output subtree name and `{registry}` expansion. Defaults to `registry`. See below. |
| `include` | list | No | Glob patterns selecting packages. Empty means everything. |
| `exclude` | list | No | Glob patterns vetoing packages. Empty by default. |
| `trusted_hosts` | list | No | Hosts exempted from the source-side network guard. Empty by default. See [`trusted_hosts`](#trusted-hosts). |

`index` must not carry credentials. A URL of the form `https://user:pass@host/` is rejected when the file loads, because a failed fetch prints the URL and CI logs travel. Use the environment variables under [Authentication](#authentication).

`index` must also be fetched over `https`, unless its host is listed in that source's [`trusted_hosts`](#trusted-hosts). The index tree is this mirror's *control plane* — `c/index.json` and every root document name the package, `content` digest, and destination `repository` a run will copy — so an on-path attacker rewrites the whole plan by editing one plaintext response, and digest verification cannot catch it, because the digests arrive in the same tampered document. A plaintext source fails when the file loads:

```
'<scheme>' is a plaintext transport, and this source's index is the control plane naming every
package and digest the run copies; use https, or add '<host>' to this source's `trusted_hosts:`
```

### `as` — and why you cannot change it later {#as}

`as` does two jobs at once. It is the directory name beneath [`output`](#output), so it is part of the URL every consumer configures. It is also what `{registry}` expands to in [`destination`](#destination), so it is part of every destination repository path.

It must be a legal OCI path component. `ocx.sh` and `ghcr.io` qualify — dots are fine. `localhost:5001` does not, and is rejected with an error naming `as` as the thing to fix. It is never silently rewritten to make it fit.

!!! danger "`as` is immutable after the first publish"

    Changing it does two irreversible things at once:

    - **It renames the served subtree.** Every machine whose config names `<output>/<old>` stops resolving. Fixing that is a coordinated change across the whole fleet.
    - **It re-homes every destination repository**, because `{registry}` expands differently. The mirror copies everything again under the new paths, and the old repositories stay where they are — this tool never deletes. You pay for the storage twice and no run will ever clean it up.

    Pick the value once. If you genuinely must change it, treat it as a new mirror: new output directory, new destination prefix, fleet reconfiguration, and manual cleanup of the old repositories.

### `include` and `exclude` {#filters}

Globs match against the two-segment package name, `<namespace>/<package>`.

The only wildcard is `*`. There is no `**`, no `?`, and no `{a,b}` alternation — `include` is already a list, so write two entries instead of one brace expression. A pattern using an unsupported character is rejected when the file loads rather than silently matching nothing.

A package is mirrored when it matches **some** `include` **and no** `exclude`. Exclude always wins:

```yaml
include: ["kubernetes/*"]
exclude: ["kubernetes/internal-*"]   # excluded, despite matching the include
```

An empty or absent `include` means every package in the source catalog.

!!! note "Narrowing a filter does not remove anything"

    The mirror is append-only. Tightening `exclude` stops *future* copies; it does not delete what previous runs already wrote. To genuinely shrink a mirror, delete the output subtree and re-run — which re-copies from scratch. Registry-side content stays regardless.

### `trusted_hosts` {#trusted-hosts}

Upstream index documents tell the mirror which registry actually holds each package's bytes. That pointer is data authored by someone else, so before dialling it the mirror refuses private, loopback, link-local and carrier-NAT addresses — otherwise a hostile or compromised index could aim the mirror at your internal network and have it fetch with your credentials.

If a legitimate upstream index points at a registry on a private address, list that host here to allow it:

```yaml
trusted_hosts: ["registry.internal.example"]
```

This guard is **source-side only**. Your own [`target`](#target) registry is not checked against it, which is why an Artifactory on an RFC1918 address works with no configuration: the destination is something you wrote in this file, not something an upstream document told the mirror to contact.

The same list is also the exemption for `index`'s own `https`-only rule above — one list, one decision for both concerns. It is what lets the acceptance harness point `index:` at `http://localhost:5001`.

## `destination` {#destination}

```yaml
destination: "{registry}/{namespace}/{package}"
```

Plain text substitution — no expressions, no conditionals, no functions. Three placeholders are defined:

| Placeholder | Expands to |
|---|---|
| `{registry}` | the source's [`as`](#as) value |
| `{namespace}` | the first segment of the package name |
| `{package}` | the second segment |

Anything else in braces is an error at load time. The result is appended to `target.repository`, so a package `kubernetes/kubectl` from a source with `as: ocx.sh` lands at `artifactory.corp.example/ocx-mirror/ocx.sh/kubernetes/kubectl`.

`{registry}` is **required when more than one source is configured** — without it, two sources publishing the same package name would collide. A single-source spec may omit it. Adding a second source to a spec that omits it turns that spec invalid, which is the point.

Two packages that would expand to the same destination are refused before anything is copied, with both package names in the error.

**Expansion is refused, never repaired.** A package name that does not fit the OCI grammar — uppercase letters, a `..` segment, whitespace, a colon — fails the run instead of being lowercased, slugified or path-cleaned. Silently normalising would let two distinct upstream names collide at one destination, and a `..` segment would write outside the prefix you configured. You get an error naming the offending name.

!!! danger "Editing `destination` after the first publish re-homes everything"

    Same one-way door as [`as`](#as), for the same reason. The next run copies every package again under the new paths and leaves the old repositories behind forever. The mirror warns when it notices that a package's recorded destination no longer matches what the current template produces — but the warning arrives after the template already changed, so treat the template as fixed once you have published.

## `on_error` {#on-error}

```yaml
on_error: continue   # or: fail_fast
```

Governs what happens when **one package** fails — a manifest that will not pull, a digest mismatch, a rejected push.

- `continue` (default) counts the failure, reports it in the summary, and carries on. The run exits non-zero if anything failed. One broken package does not abort 120 healthy ones.
- `fail_fast` stops at the first failed package.

`--fail-fast` on the command line overrides the file.

**It does not govern everything.** If the destination registry answers a query with something that is not a definite yes or no — a 503, a timeout, an authentication failure — the whole run aborts immediately under either setting. The mirror decides whether to upload a blob by asking the destination whether it already has it; an answer it cannot trust must never be read as "absent", or a flaky link would re-upload the entire catalog. The same applies to an unreachable source index.

## `concurrency` {#concurrency}

```yaml
concurrency:
  max_blobs: 4
  max_retries: 3
```

| Key | Type | Default | Purpose |
|---|---|---|---|
| `max_blobs` | integer | 4 | How many blobs are copied at once |
| `max_retries` | integer | 3 | Extra attempts after a rate-limit response |

`max_blobs` defaults to 4 because each in-flight blob is held in memory while it is verified. Some published assets exceed 200 MB, so raising this raises peak memory roughly in proportion. Lower it on a small runner; raise it only if you know your largest blob.

Retries are reactive: they fire only on an HTTP 429 from a pull or a push, backing off from one second and doubling on each attempt up to a thirty-second cap — deterministic, no jitter. The registry's `Retry-After` header is not read; the doubling ladder stands in for it. There is no proactive throttle.

## Authentication {#authentication}

**No credentials belong in this file.** Any credential-shaped key — `password`, `token`, `username`, `auth`, `credentials`, `secret`, `api_key` — is refused when the file loads, at any nesting depth, including in a file it `extends`. The error names the key and the environment variable to use instead, and never prints the value.

Credentials come from the environment, resolved in this order:

1. `OCX_AUTH_<slug>_TYPE`, `OCX_AUTH_<slug>_USER`, `OCX_AUTH_<slug>_TOKEN`
2. the Docker credential store
3. anonymous

`<slug>` is derived from the registry host. This is the same mechanism `ocx` itself uses, so a machine already able to pull from a registry can already push to it.

## A corporate mirror, end to end {#example}

Mirror two upstream namespaces into an Artifactory OCI repository, and serve the resulting index from the same repository's Pages site.

```yaml
kind: registry

target:
  registry: artifactory.corp.example
  repository: ocx-mirror

output: ./public

destination: "{registry}/{namespace}/{package}"

on_error: continue

sources:
  - registry: ocx.sh
    index: https://index.ocx.sh
    as: ocx.sh
    include:
      - "kubernetes/*"
      - "hashicorp/*"
```

Run it:

```sh
ocx-mirror registry sync
```

Consumers point at the published tree:

```toml
[registries."ocx.sh"]
index = "https://pages.corp.example/ocx.sh"
```

### The CI job {#ci}

There is no workflow generator for this verb, deliberately — you run one command, in whatever CI you already have. A scheduled GitHub Actions job is four steps:

```yaml
name: mirror
on:
  schedule: [{ cron: "0 3 * * *" }]
  workflow_dispatch:

concurrency:
  group: registry-mirror
  cancel-in-progress: false

jobs:
  sync:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: ocx-sh/setup-ocx@v1
      - run: ocx-mirror registry sync
        env:
          OCX_AUTH_ARTIFACTORY_CORP_EXAMPLE_USER: ${{ secrets.MIRROR_USER }}
          OCX_AUTH_ARTIFACTORY_CORP_EXAMPLE_TOKEN: ${{ secrets.MIRROR_TOKEN }}
      - run: |
          git add public
          git diff --cached --quiet || git commit -m "mirror: sync"
          git push
```

The same three lines work unchanged in GitLab CI, Jenkins, or a cron box with a deploy key.

!!! warning "The `concurrency:` group is your responsibility"

    Two runs against the same output tree at the same time will race on individual package documents. The mirror serialises the final catalog write, but not the whole run, so it cannot prevent this on its own. Whatever CI system you use, make sure a second run cannot start while the first is still going — the `concurrency:` block above is how GitHub Actions expresses it, and `cancel-in-progress: false` matters: a mirror run must be allowed to finish, not be killed halfway.

## Running it {#running}

```sh
ocx-mirror registry sync [SPEC]
```

`SPEC` defaults to `./registry.yml`. Every flag is listed in the [CLI reference][cli-registry-sync].

Every run prints a summary line, including a run that did nothing:

```
121 total, 0 copied, 121 skipped, 0 failed
```

Silence would be indistinguishable from a job that never started.

### What a re-run does {#incremental}

A package is skipped only when the mirror can confirm it is fully present: its document exists, every upstream tag is recorded against the same content, and the catalog agrees. Anything else is re-copied — and re-copying is cheap, because every blob already at the destination is skipped after a single query.

Before checking packages individually, each source gets a cheaper test first: if the source's catalog is byte-identical to the last fully successful run **and** nothing new has been added to `include:`, the whole source is skipped in one request and the run prints `<as>: unchanged since the last run — nothing to compare` instead of a package table. Only a source that fails this test falls through to the per-package check above.

That is also the repair mechanism. A run interrupted halfway leaves content at the destination that nothing points at yet, which is harmless to consumers; the next run finishes the job. A package only becomes visible in the index after every byte it names is confirmed at the destination, so consumers never see a package that is not fully there.

### Recovering a damaged tree {#repair}

If the index tree's catalog is wrong — hand-edited, drifted from the package documents beside it, or the catalog file itself is corrupt:

```sh
ocx-mirror registry sync --repair-catalog
```

This rebuilds `c/index.json` from the package documents already on disk under `p/`. It is not part of a normal run: it walks every package in the tree, including ones your current filters exclude. Reach for it when the catalog is the problem, not on a schedule.

!!! warning "A truncated or unparseable package document defeats `--repair-catalog` — delete it instead"

    `--repair-catalog` reads every package document under `p/` to rebuild the catalog, and a single one it cannot parse aborts the whole rebuild before any entry is corrected — healthy packages included. An ordinary `registry sync` run does no better: it fails only that one package, permanently, since nothing in a normal run rewrites a document that is already on disk. The only recovery is to delete the damaged `p/<namespace>/<package>.json` file by hand. The next `registry sync` run then re-copies that package from the source and writes a fresh document in its place — `--repair-catalog` alone does not fetch anything, so it cannot restore a document that is gone.

### Seeing what a run would do {#dry-run}

```sh
ocx-mirror registry sync --dry-run
```

Reports the packages that would be copied and the number of bytes that would transfer, and copies nothing. The byte figure is not an estimate: it asks the destination which blobs it is missing and sums their recorded sizes.

## Things this does not do {#limits}

- **It never deletes.** No pruning verb, no way to remove a package or a tag. A tag that upstream retires stays in your mirror forever; that is the same property that stops a transient upstream failure from silently removing a version your fleet is pinned to.
- **It does not copy signatures or attestations.** A package carrying them fails with an error naming up to 10 of the referrer digests found and the total count, rather than being mirrored silently incomplete. Signature mirroring arrives with signing support.
- **It does not filter by version.** A mirrored package brings its whole tag set. Copying a subset would leave rolling tags like `latest` pointing at versions you never copied.

<!-- internal -->
[cli-registry-sync]: ./cli.md#registry-sync
