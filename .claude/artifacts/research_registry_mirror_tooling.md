# Research: registry-mirroring tooling and replication-rule design

**Axis:** design-pattern precedent + product / competitive landscape
**Date:** 2026-08-13
**For:** `adr_registry_mirror_sync.md` (`ocx-mirror registry sync`)
**Method:** three parallel web-research passes, sources cited inline. Conducted
during the design conversation that produced the ADR; persisted here because a
design record must not rest on findings that live only in a transcript.

---

## 1. The finding that shapes the whole design

**Registry-wide enumeration is the unsolved problem of every comparable tool,
and OCX does not have it.**

Every OCI mirroring tool that can mirror "a whole registry" does it through the
Docker Distribution `_catalog` API, and that endpoint is unreliable in practice:

| Tool | Registry-wide enumeration | Evidence |
|---|---|---|
| skopeo sync | **Not supported at all.** Every repository must be named explicitly. | Feature request [containers/skopeo#364](https://github.com/containers/skopeo/issues/364), open 6+ years |
| regsync | `type: registry` — "requires the registry to support the `_catalog` API" | [regclient.org/usage/regsync](https://regclient.org/usage/regsync/) |
| dregsy | `lister: catalog` (default), plus `dockerhub` / `index` listers; **self-described alpha** | [design-image-matching.md](https://github.com/xelalexv/dregsy/blob/master/doc/design-image-matching.md) |
| zot sync | Polled mode enumerates upstream; docs warn it does not work against Docker Hub (no catalog listing + rate limits) | [zot mirroring guide](https://zotregistry.dev/v2.1.3/articles/mirroring/) |
| crane / gcrane / oras | argv-only (`-r`, `--all-tags`); no config, no filter | [crane_copy.md](https://github.com/google/go-containerregistry/blob/main/cmd/crane/doc/crane_copy.md) |

And `_catalog` fails in the field: [xelalexv/dregsy#57](https://github.com/xelalexv/dregsy/issues/57)
— a wildcard catalog sync against Quay dies with
`INVALID_REQUEST: Unable to decode repository and actions: registry:catalog:*`,
because Quay rejects the wildcard auth scope dregsy requests. Unresolved; the
documented workaround is to name every repository by hand. Docker Hub does not
expose `_catalog` at all.

**OCX enumerates from `c/index.json` — a static file.** No `_catalog`, no auth
scope negotiation, no rate limit on discovery, and the list contains exactly the
packages someone deliberately published rather than every blob in the registry.
This is the structural reason an OCX registry mirror is not "wrap `skopeo sync`",
and it should be stated as such in the ADR's rationale.

---

## 2. Filter grammar — the ecosystem has converged

**Glob for repository/package names, regex for tags.** Unanimous across the
server products; the client tools that chose regex for names did so before the
convention settled.

| Product | Name filter | Tag filter |
|---|---|---|
| Harbor | glob: `*` (non-`/`), `**` (crosses `/`), `?`, `{a,b}` alternation | glob, with a matching/excluding toggle |
| Artifactory | ANT-style include/exclude patterns, default include `**/*` | n/a (path-based) |
| zot sync | glob `prefix` (`*`, `**`) | `tags: {regex, excludeRegex, semver}` |
| regsync | regex, auto-anchored `^…$` | regex `allow`/`deny` + `semverRange` |
| dregsy | regex (`regex:` prefix) on `from` | verbatim / `semver:` / `regex:` / `keep:` |
| Quay | n/a (one repo per mirror object) | shell wildcards, e.g. `1-1,1-2,1-*` |
| ECR pull-through | prefix→prefix map only | n/a |

Sources: [Harbor replication rules](https://goharbor.io/docs/main/administration/configuring-replication/create-replication-rules/),
[Artifactory include/exclude](https://jfrog.com/help/r/how-to-use-include-exclude-patterns),
[zot sync config](https://pkg.go.dev/zotregistry.dev/zot/v2/pkg/extensions/config/sync),
[regsync](https://regclient.org/usage/regsync/),
[dregsy](https://github.com/xelalexv/dregsy/blob/master/README.md).

### Precedence: state it, because the reference implementation didn't

Artifactory publishes the cleanest rule, verbatim: an artifact passes if its name
*"matches any of the include patterns, **and does not match any of the exclude
patterns**"* — a boolean AND-NOT, i.e. **exclude is a veto**.

regsync's own documentation contradicts itself on exactly this point: *"A tag
much match each criteria to be included, so a rule to `allow: ["latest"]` and
`deny: ["dev"]` would not match any tags."* That sentence is incoherent under any
plain reading (`latest` matches neither pattern in `deny`), and it is what is
actually published — retrieved identically on two independent fetches of
[regclient.org/usage/regsync](https://regclient.org/usage/regsync/).

**Implication for the ADR:** precedence must be stated in one sentence in the
schema and pinned by a fixture. Do not inherit it by analogy from any single
tool.

---

## 3. Destination naming — collision is the universal footgun

| Product | Rewrite primitive | Preserves hierarchy | Rename possible |
|---|---|---|---|
| Harbor | destination namespace + **flatten N levels** | above the flatten cutoff only | no |
| zot | `prefix` match → `destination` concat, `stripPrefix` | yes (wildcard remainder appended) | yes, for literal prefixes |
| regsync | Go `text/template` `target:` (`.Ref.Registry`, `.Ref.Repository`, `.Ref.Tag`, `env`, `file`) | fully manual | yes |
| dregsy | regex capture-group substitution in `to:` | yes | yes |
| skopeo sync | **none** — flattens to basename; `--scoped` re-prefixes the source path | only with `--scoped` | no |
| Artifactory replication | none (`Path Prefix` scopes, does not rewrite) | identity | no |
| Nexus | none — proxy is always 1:1 | identity | no |
| ECR pull-through | `ecrRepositoryPrefix` ↔ `upstreamRepositoryPrefix` map | yes (remainder kept) | yes |

**skopeo sync flattens to the image basename by default**, so `registryA/app` and
`registryB/app` both land at `app` and silently overwrite. `--scoped` exists
solely to undo that ([skopeo-sync man page](https://github.com/containers/skopeo/blob/main/docs/skopeo-sync.1.md)).
Harbor's flatten-N-levels setting is the same problem from the other direction.

**Collision policy is undocumented by every one of the seven products surveyed.**
Harbor's `Override` checkbox is the closest anything gets, and its behaviour when
unset is not stated. zot's is an emergent "last entry processed wins". ECR makes
the case structurally unreachable rather than specifying it.

**Implication for the ADR:** mandating `{registry}` in the destination template
makes collisions structurally impossible for the multi-source case, and refusing
a template that omits it at spec-load is a validation rule no competitor has.
Stating the policy plainly is a differentiator, not a gap.

Also note **regsync's template vocabulary is far richer than we need** (full Go
`text/template`, `env`/`file` functions). Three placeholders and plain string
substitution avoids a template-engine dependency; add expressiveness only on
demand.

---

## 4. Deletion propagation is opt-in or absent, everywhere

| Product | Deletion propagation |
|---|---|
| Harbor | never for manual/scheduled rules; opt-in checkbox for event-based only |
| Artifactory | opt-in `Sync Deleted Artifacts` |
| zot | **not implemented** — [project-zot/zot#3102](https://github.com/project-zot/zot/issues/3102) open |
| Quay | explicitly archival by design; content drops only when the operator narrows the tag filter |
| ECR PTC | cache-expiry semantics, not deletion propagation |

**Implication for the ADR:** OCX's merge-only-never-delete index discipline lands
exactly on the industry default. "The mirror is append-only" is not a limitation
to apologise for; it is the settled convention. A prune capability, if built, is
an explicit opt-in verb.

---

## 5. Mirror vs cache is a first-class distinction, not a spectrum

The two OCI-native products both model these as **separate things**, not as
degrees of one setting:

- **zot** — `onDemand: true` is a pull-through cache; `onDemand: false` +
  `pollInterval` is "full mirror mode". Different modes, documented separately.
- **Quay** — repository *mirroring* (persistent scheduled copy) and *proxy cache*
  repos (transparent passthrough, no persistent copy) are two distinct product
  features.

**Implication for the ADR:** the owner's rejection of the Artifactory-style
caching proxy is not a preference; it is where both OCI-native implementations
drew the line.

---

## 6. Ordering guarantees are weaker than assumed

The OCI Distribution Spec only says blobs are *"typically"* pushed before the
manifest, and a registry **MAY** — not MUST — reject a manifest referencing
missing blobs (`MANIFEST_BLOB_UNKNOWN`)
([distribution-spec](https://github.com/opencontainers/distribution-spec/blob/main/spec.md)).
None of Harbor, zot, or Artifactory documents its own internal ordering
guarantee.

**Implication for the ADR:** "write the index root only after the content push
returns" is a guarantee this design can actually make, because the root document
is the only thing that makes a package resolvable. Every surveyed competitor
lacks an equivalent transaction boundary. Worth stating as a named property.

---

## 7. Retry, throttling, chunking — the knobs that exist elsewhere

- **zot**: per-registry `maxRetries`, `retryDelay`, `maxRetryDelay` — the most
  granular of the surveyed set.
- **Harbor**: `Copy by chunk` (Harbor→Harbor only, default 10 MB, tunable via
  `REPLICATION_CHUNK_SIZE`), bandwidth limit in KB/s (`-1` unlimited),
  "single active replication" to prevent overlapping runs, auto-retry with
  undocumented count.
- **regsync**: `ratelimit: {min, retry}` self-throttle — pauses when a registry's
  remaining-pull budget drops below `min`. Docker Hub is the motivating case.
- **Artifactory**: no documented replication retry policy found.

---

## 8. JFrog Artifactory as the destination — hard requirements

The corporate destination in the motivating deployment is Artifactory. Findings
that constrain the design:

| Topic | Finding |
|---|---|
| **Repository type** | The destination **must be an OCI-type repository** (shipped in **7.74**), not a legacy Docker-type repo-key. Pushing non-image OCI artifacts into a Docker-type repo produces `unable to upload blob … unknown: Not Found` — [Red Hat KB 7084299](https://access.redhat.com/solutions/7084299) documents the format-mismatch diagnosis. |
| **Custom media types** | Accepted. [kubewarden/kwctl#59](https://github.com/kubewarden/kwctl/issues/59) is the proof: a fully custom WASM config (`application/vnd.wasm.config.v1+json`) + layer media type got a 406 from one client and pushed cleanly via ORAS to the *same* Artifactory instance — a client bug, not a registry rejection. No config-media-type allowlist is documented anywhere. |
| **Xray scanning** | Does not cover non-image OCI payloads. Storage/push/pull unaffected — a feature gap, not a rejection ([oci-repositories docs](https://docs.jfrog.com/artifactory/docs/oci-repositories)). |
| **Referrers (OCI 1.1)** | `GET /v2/<name>/referrers/<digest>` from **7.90.1+**, and scoped to **OCI and Helm-OCI repository types only** — not plain Docker-type repos ([conformance blog](https://jfrog.com/blog/full-conformance-to-oci-v1-1/)). Fallback-tag behaviour undocumented. Relevant to [ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7) (re-sign + attest mirrored bundles): choosing OCI-type now avoids a later repo migration. |
| **Path grammar** | Repo-key (first segment): lowercase, no underscore, ≤63 chars, no `jfrog-*` prefix, no `-cache` suffix. Everything after it follows the distribution-spec name grammar `[a-z0-9]+(?:[._-][a-z0-9]+)*` per component — **dots legal** (`ocx.sh`, `ghcr.io`), **uppercase illegal**, total <256 chars. The grammar is spec-derived rather than restated by JFrog; corroborated by JFrog's own `ghcr.io`-namespaced remote-repo walkthrough. |
| **Auth** | API keys **deprecated at 7.98** (creation blocked). Use scoped access tokens, `artifact:<repo-key>:w`; default scope `applied-permissions/user`. `docker login` writes the standard credential store; Artifactory's Docker/OCI auth is the ordinary Basic→Bearer challenge flow, so a non-Docker OCI client reading `~/.docker/config.json` works — documented by construction rather than by an explicit JFrog statement. |
| **Cross-repo blob mount** | **Undocumented** either way. But Artifactory's [checksum-based storage](https://docs.jfrog.com/installation/docs/checksum-based-storage-implementation) stores one filestore entry per checksum regardless of how many repos reference it, so a duplicate push costs wire transfer only, never storage. |
| **Large-blob upload** | Timeouts are generic infrastructure (Tomcat `connectionTimeout`, reverse-proxy `proxy_read_timeout`), not OCI-specific. Multi-chunk `PATCH` data loss is a **real bug class on other registries** — ECR stores only the first chunk, [aws/containers-roadmap#2831](https://github.com/aws/containers-roadmap/issues/2831) — with no Artifactory-specific report found either way. Untested here; worth a direct probe. |

---

## 9. Vocabulary worth borrowing

- `include` / `exclude` (Artifactory, Harbor) over `allow` / `deny` (regsync) —
  the former pair is what operators of server products already read.
- `destination` (zot's actual field name; **not** `destPrefix`, which no version
  of zot's docs ever used).
- Harbor's three-way trigger vocabulary: `manual` / `scheduled` / `event`.
- `--dry-run` as the universal preview verb (regsync `check`, skopeo `--dry-run`,
  Harbor's execution preview).

## 10. What this design does that none of the seven do

1. Enumerates from a published static catalog instead of `_catalog` (§1).
2. States its destination-collision policy, and enforces it structurally by
   requiring `{registry}` in the template (§3).
3. Offers a real atomic-visibility guarantee, because the index root — not the
   registry tag — is what makes a package resolvable (§6).

Each is a rationale line the ADR should carry, not a feature bullet.
