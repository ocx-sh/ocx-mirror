# dist.yml Reference

`dist.yml` describes a **distribution mirror**: it copies the OCX *bootstrap layer* — the `ocx` release archives and the `dist.json` manifest that names them — into a store your network can reach, and rewrites the manifest so every consumer downloads from your copy.

It is consumed by `ocx-mirror dist sync`. One repository holds exactly one `dist.yml`.

The three spec files describe different jobs:

| File | Job |
|------|-----|
| [`mirror.yml`](mirror-yml.md) | Package an upstream tool's release archives into an OCX package you publish |
| [`registry.yml`](registry-yml.md) | Copy OCX packages someone else already published into your registry |
| `dist.yml` | Copy **ocx itself** — the binary and its manifest — so a machine with no route to `github.com` can install it |

Nothing here touches an OCI registry. The output is plain files over plain HTTP, because that is all the bootstrap path can speak: `install.sh` runs `curl` with no token and no `jq`, and the Bazel and CMake integrations use `ctx.download` and `file(DOWNLOAD)`.

## Who reads the output {#consumers}

Every consumer of a mirrored distribution is configured with the same two environment variables:

| Variable | Points at |
|----------|-----------|
| `OCX_INSTALL_DIST_URL` | your `dist.json` |
| `OCX_INSTALL_MIRROR_URL` | your archive base (only needed when the manifest was *not* rewritten — see [`publish`](#publish)) |

They are read by the five shell installers on `setup.ocx.sh`, by `rules_ocx` (`ocx/private/download.bzl`), by `find_ocx` (`ocx.cmake`), and by the OCX SDKs.

!!! warning "The mirror must allow anonymous reads"

    None of those consumers can send credentials: `curl` in `install.sh` has no auth knob, `file(DOWNLOAD)` has none by default, and `ctx.download` needs netrc. A store that requires a token on `GET` will fail every bootstrap, and the failure looks like a network error. Authenticated *reads* are a planned follow-up; authenticated **writes** are supported today via [`upload`](#upload).

## Top-level keys {#top-level}

| Key | Type | Required | Purpose |
|-----|------|----------|---------|
| `kind` | string | Yes | Must be `dist`. Distinguishes this file from a `mirror.yml` or a `registry.yml`. See [`kind`](#kind). |
| `source` | URL | No | Upstream manifest to mirror. Defaults to `https://setup.ocx.sh/dist.json`. See [`source`](#source). |
| `output` | path | Yes | Directory the mirror tree is written into. See [`output`](#output). |
| `select` | object | No | Which upstream releases to keep. See [`select`](#select). |
| `publish` | object | Yes | Where the copy will be served from, and under what path shape. See [`publish`](#publish). |
| `upload` | object | No | Optional native HTTP `PUT` of the emitted tree. See [`upload`](#upload). |
| `trusted_hosts` | array | No | Hosts reachable over plaintext `http://`. See [`trusted_hosts`](#trusted-hosts). |

`extends:` works the same way it does in the other two specs — a base file is shallow-merged and the child's keys win.

## `kind` {#kind}

```yaml
kind: dist
```

Mandatory, and the value must be exactly `dist`. It is read before anything else, so pointing `dist sync` at a `registry.yml` reports the actual problem instead of an unknown-field error about a key that happens to be checked first.

## `source` {#source}

```yaml
source: https://setup.ocx.sh/dist.json
```

The upstream manifest, fetched on every run. It is **never re-derived from the GitHub Releases API**: target extraction, channel semantics and the `latest` pointers live in `gen-dist.sh` on the publishing side, and a second implementation here would drift from it silently and invisibly.

The manifest is the control plane naming every version and digest the run trusts, so `https` is required unless the host is listed in [`trusted_hosts`](#trusted-hosts). A URL embedding userinfo (`https://user:pass@host/`) is refused rather than stripped.

The body is capped at **8 MiB**, refused on a declared oversize `Content-Length` and again while streaming, so an endpoint that omits or understates the header cannot buffer more than that into memory. A real `dist.json` is a few hundred bytes per release and target, three orders of magnitude below the cap.

## `output` {#output}

```yaml
output: ./public
```

Directory the mirror tree is written into. Always written, whether or not [`upload`](#upload) is configured — a tree plus the operator's own `aws s3 sync`, `rsync`, `jf rt upload` or commit step is the path that works against every store, including the ones that need request signing.

The tree looks like this, for the default layout:

```text
public/
├── dist.json                       # rolling manifest — what OCX_INSTALL_DIST_URL points at
├── dist.json.sha256                # sha256sum-format sidecar for the rolling manifest
├── dist/
│   └── <sha256>.json               # immutable, content-addressed snapshot
└── v0.5.8/
    ├── ocx-x86_64-unknown-linux-gnu.tar.gz
    └── ocx-aarch64-apple-darwin.tar.gz
```

The manifest file names are fixed and not configurable: `OCX_INSTALL_DIST_URL` is set once per consumer and must not move when [`layout`](#publish) changes.

### Reproducible installs {#snapshots}

`dist.json` is a rolling pointer and changes whenever upstream publishes anything. `dist/<sha256>.json` never changes. Pinning is therefore just:

```sh
OCX_INSTALL_DIST_URL=https://art.corp.example/ocx-dist/dist/a1b2c3….json
```

Because every release row carries an inline `sha256`, pinning the manifest pins the entire closure — one hash, fully reproducible install. Snapshots are content-addressed rather than timestamped, so a run whose manifest did not change writes no new file, and the name is verifiable rather than merely unique.

## `select` {#select}

```yaml
select:
  min_version: "1.0.0"
```

Which upstream releases survive into the mirror. Every filter is **subtractive** and they combine with **AND** — a release is kept if and only if every filter present accepts it. That rule is fixed, so filters added later compose without a precedence rule to learn.

| Key | Type | Purpose |
|-----|------|---------|
| `min_version` | string | Inclusive lower bound |

`min_version` is semver-ordered, so `min_version: "1.0.0"` **excludes** `1.0.0-rc.1` — a prerelease sorts below its own release.

Filtering also re-points `latest` and `latest_next` at the newest surviving release in each channel, and a channel that empties becomes an explicit `null`. Without that, a mirror holding only `1.x` would still advertise a `latest` it never downloaded.

A bound that leaves **no** release at all **fails the run** instead of publishing an empty manifest — see [clobber-safety](#clobber-safety). The reachable cause is a typo in `min_version`, and nothing else about the run would look wrong.

Omitting `select` mirrors everything, which is the simplest correct configuration and the one to start with.

## `publish` {#publish}

```yaml
publish:
  base_url: https://art.corp.example/ocx-dist
  layout: "{tag}/{filename}"
```

| Key | Type | Required | Purpose |
|-----|------|----------|---------|
| `base_url` | URL | Yes | Public base every mirrored `url` is composed from. Trailing slashes are ignored; a query or fragment is refused. |
| `layout` | string | No | Path shape below `base_url`. Defaults to `{tag}/{filename}`. |

`layout` is plain substitution over five placeholders — `{version}`, `{tag}`, `{target}`, `{filename}`, `{channel}` — with no template engine. An unknown placeholder is a load error rather than an empty string, because an empty expansion would collapse a path segment and quietly collide every release onto one path.

**One path per release and target.** A layout that renders two different rows to the same path — `"{channel}/ocx.tar.gz"`, say — fails the run naming both releases. Overwriting would leave the manifest pointing one URL at two targets, which installs the wrong binary with nothing in the tree to show for it.

The **same** rendered path is used three times: the file written under `output:`, the `url` stamped into the mirrored manifest, and the `PUT` target. They cannot disagree about where a byte lives.

### Why the manifest is rewritten {#rewrite}

The mirrored `dist.json` carries your URLs, not upstream's. It is required for any store whose path shape differs from what the installers compose, and applied unconditionally so that one code path serves both cases.

The installers compose a mirror URL as `${OCX_INSTALL_MIRROR_URL}/${tag}/${filename}`. For the default layout on a plain file store that composition already works, and `OCX_INSTALL_MIRROR_URL` alone would do — but a package registry does not match it. A GitLab generic package registry addresses files as:

```text
https://gitlab.corp.example/api/v4/projects/42/packages/generic/ocx/<version>/<file>
```

Teaching every consumer a URL template would mean placeholder substitution in five shell dialects plus Bazel, CMake and every SDK. Rewriting once, here, means every consumer works unchanged and needs only `OCX_INSTALL_DIST_URL`.

`sha256` is never touched by the rewrite. The mirror is untrusted transport: a swapped archive fails verification whatever host served it.

```yaml
# GitLab generic package registry
publish:
  base_url: https://gitlab.corp.example/api/v4/projects/42/packages/generic/ocx
  layout: "{version}/{filename}"
```

Like `source`, `base_url` must be `https` unless its host is trusted, and must not embed userinfo — a credential there would be copied into every manifest row and served to every consumer.

**No query or fragment either.** Two consumers compose onto this base — the published URL and the upload target — and they treat a query differently, so the same byte would be advertised at one URL and stored at another. It is also the shape a credential arrives in: an Azure Blob SAS *is* a query string, and a base carrying one would put a live write credential into a manifest served to everybody while the upload itself still succeeded. Put such a credential in [`identity`](#identity) or `headers` instead.

## `upload` {#upload}

```yaml
upload:
  identity:
    type: basic
    username_env: ART_USER
    password_env: ART_PASSWORD
  retry_delays: [1, 5, 10, 30, 60]
  headers:
    x-ms-blob-type: BlockBlob
```

Optional. Omit it to emit the tree and ship it yourself.

One `PUT` implementation covers Artifactory generic repositories, Nexus raw repositories and GitLab generic packages — they are the same request, and all three create intermediate directories on the way. Azure Blob differs by one header, which is what `headers` is for. Stores that need request signing (S3, GCS) are served by the emitted tree and their own CLI.

!!! warning "Not WebDAV, under the default layout"

    [RFC 4918 §9.7.1](https://www.rfc-editor.org/rfc/rfc4918#section-9.7.1) forbids `PUT` from creating collections, so a WebDAV server answers `409` when the parent is absent. The default `{tag}/{filename}` always has one, and `409` is a `4xx`, which is never retried — the first run fails hard. Use WebDAV only with a flat `layout`, or create the collections ahead of the run.

| Key | Type | Purpose |
|-----|------|---------|
| `identity` | object | Credentials, resolved from the environment. Omit for a store that accepts anonymous writes. See [`identity`](#identity). |
| `retry_delays` | array of integers | Backoff schedule in seconds. Default `[1, 5, 10, 30, 60]`; `[]` disables retry. |
| `headers` | map | Extra request headers, sent verbatim on every `PUT`. `Authorization` is refused — it belongs in `identity`. |

**The array length is the retry count.** There is deliberately no separate `max_retries` key that could contradict it.

Retries cover transport errors, timeouts, `5xx` and `429` only. A `4xx` is **never** retried: a `401` or `403` is a credential problem a retry cannot solve, and hammering one burns the backoff window and trips account-lockout policy. A server-supplied `Retry-After` in seconds is honoured when it exceeds the scheduled delay, clamped to 300 s so a throttling store cannot turn a five-step backoff into a multi-hour CI hang.

### `identity` {#identity}

```yaml
identity:
  type: bearer
  token_env: ART_TOKEN
```

```yaml
identity:
  type: basic
  username_env: ART_USER
  password_env: ART_PASSWORD
```

Tagged by `type`, so `token_env` under `type: basic` is a load error rather than a silently ignored key — the invalid combinations are unrepresentable instead of being a rule someone forgets.

**Every field names an environment variable; none holds a value.** There is no literal variant, so a credential cannot reach a committed spec even by accident. The block is spelled `identity:` rather than `auth:` because a key named `auth` is refused at any depth by the same credential guard that protects `registry.yml`, and weakening it to admit a block holding no secrets would weaken it for the blocks that do.

A named variable that is unset or empty fails the run before the first byte moves — credentials resolve ahead of the manifest fetch, so a typo costs seconds rather than a repeated multi-gigabyte download.

**Redirects are refused on the upload path.** `reqwest` drops `Authorization` when a redirect crosses origins, but it cannot know that a header you configured is a credential too — `JOB-TOKEN` and `X-JFrog-Art-Api` are ordinary headers to it, and would be replayed to whatever host a `Location` names. A store that answers a `PUT` with a redirect is therefore not supported. Downloads still follow redirects: no credential is attached to them, and the manifest digest is what makes those bytes trustworthy.

### Idempotency and publish order {#ordering}

Every file is `HEAD`ed before it is `PUT`. The destination is the authority, so a file deleted from the store is re-uploaded by the next run instead of being skipped forever by stale local state. `X-Checksum-Sha256` rides along on archive uploads, which lets Artifactory link an already-stored blob instead of re-reading the body.

Uploads run in a fixed order — **archives, then the content-addressed snapshot, then the sidecar, then `dist.json` last**. A consumer reading mid-run therefore resolves either the old manifest or the new one, and both are fully backed by bytes already in the store.

!!! note "GitLab generic packages are immutable by default"

    Re-publishing the rolling `dist.json` needs duplicate publishing enabled for generic packages on the project. Content-addressed snapshots never collide, so they work either way.

## `trusted_hosts` {#trusted-hosts}

```yaml
trusted_hosts:
  - 127.0.0.1
  - 10.0.0.0/8
```

Hosts allowed to be reached over plaintext `http://`. Entries are exact hostnames or CIDR blocks. Plaintext is refused by default for the same reason as in `registry.yml`: the manifest names every version and digest the run trusts.

## Clobber-safety {#clobber-safety}

A run that cannot place **every** selected archive writes **no manifest at all** — not a partial one. The destination keeps its previous, internally consistent manifest rather than gaining one that promises archives the store does not hold. Archives that did land stay on disk and in the store, so the corrected re-run is cheap.

The rule has a second half: a run that selected **nothing** also publishes nothing. It has trivially placed every selected archive, so the partial-run guard alone would read it as a success and overwrite a working `dist.json` with one naming no releases — leaving every consumer resolving `latest` to `null`.

Both mirror the rule `gen-dist.sh` enforces on the publishing side, for the same reason.

## Complete example {#example}

```yaml
kind: dist

source: https://setup.ocx.sh/dist.json
output: ./public

select:
  min_version: "1.0.0"

publish:
  base_url: https://art.corp.example/artifactory/ocx-dist
  layout: "{tag}/{filename}"

upload:
  identity:
    type: basic
    username_env: ART_USER
    password_env: ART_PASSWORD
```

```sh
export ART_USER=ci-mirror ART_PASSWORD="$ARTIFACTORY_TOKEN"
ocx-mirror dist sync
```

Consumers then need one variable:

```sh
export OCX_INSTALL_DIST_URL=https://art.corp.example/artifactory/ocx-dist/dist.json
curl -fsSL https://setup.ocx.sh/sh | sh
```

In a fully disconnected network the installer script itself is served from the same store, since `setup.ocx.sh` is unreachable — copy `install.sh` beside `dist.json` and paste its URL instead.
