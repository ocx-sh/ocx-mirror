# ADR: `ocx-mirror registry sync` — whole-registry mirroring into a corporate registry

## Metadata

**Status:** Proposed
**Date:** 2026-08-13
**Deciders:** Michael Herwig (maintainer), architect
**GitHub Issue:** N/A — closes the `TODO(registry ADR)` at `src/command/registry/mod.rs:18-19` and fills
item 5 of [`adr_cli_namespace_restructure.md`](./adr_cli_namespace_restructure.md)'s implementation plan
(`:170`)
**Related Design Spec:** N/A (to follow)
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio, clap) and conventions in
  [`.claude/rules/subsystem-mirror.md`](../rules/subsystem-mirror.md)
**Domain Tags:** cli, spec, oci, index, security
**Supersedes:** the **content-copy half** of `external/ocx/.claude/artifacts/adr_oci_registry_mirror.md`
(see *Reconciliation* below)
**Superseded By:** N/A
**Depends on:** ocx ≥ **0.5.8** (`external/ocx` submodule bump — hard prerequisite, see *Migration*)

---

## Context

### The deployment this exists for

A corporate network blocks egress to `ocx.sh`, `ghcr.io`, `docker.io`. An operator runs an internal
OCI registry (JFrog Artifactory in the motivating case). They want their developers to
`ocx install kubernetes/kubectl:1.31` and have every byte — index documents *and* OCI content — come
from inside the perimeter. Nothing may reach out.

### What already ships, and why it is not enough

Two thirds of this problem are already solved, in ocx, and the honest baseline is to say so:

| Shipped | What it does | Why it does not close the air-gap |
|---|---|---|
| `[mirrors]` config (`adr_oci_registry_mirror.md`) | Rewrites the OCI read path per upstream host onto a corporate proxy | Requires the corporate registry to be a **pull-through proxy with egress**. An air-gapped registry has none — there is nothing to proxy through |
| `ocx index sync <REGISTRY>...` (v0.5.8) | Snapshots a source's whole catalog into a locally-servable index tree. `adr_servable_index_snapshot.md:535`: *"An air-gapped mirror is one command, then a static file server. Zero `ocx-mirror` code."* | Copies **index documents only**. Every `repository` still points at `ghcr.io`. A consumer resolves the tag correctly and then fails to fetch a single blob |

**The remaining third is the whole of this ADR**: copy the OCI content by digest into the corporate
registry, and rewrite each root document's `repository` so the index the consumer reads points at
where the bytes actually are.

### Neither shipped verb is a shortcut past the copy engine — stated, because a reader will assume otherwise

`ocx index sync` is **index-pointer-only**. It enumerates a registry's catalog and feeds every package
through the same merge-only write path as `ocx index update <pkg>`, refreshing roots and dispatch
objects into `$OCX_HOME/index/<source>/`. Its purpose is a *truthful local cache of what the registry
already says* — so by construction it never copies a blob and never rewrites `repository`.
`adr_servable_index_snapshot.md:282-286`: *"a merge-never-deletes operation is not a mirror."*

`regenerate_catalog` does not help either. Its stated invariant (`regenerate.rs:19-22`) is that it
re-derives `c/index.json` from **unchanged root bytes already on disk**; `c/index.json` is the only
path whose bytes it writes, it never writes `config.json`, and it removes no root and no `o/` object.

**Consequence for the design:** the rewritten root is bytes *this* tool constructs, written through
this tool's own call to the index store. Nothing upstream produces them.

### Why this is not "wrap `skopeo sync`"

Every comparable tool that can mirror "a whole registry" enumerates through the Docker Distribution
`_catalog` API, and that endpoint fails in the field — Quay rejects the wildcard auth scope
([xelalexv/dregsy#57](https://github.com/xelalexv/dregsy/issues/57)), Docker Hub does not expose it,
skopeo never implemented registry-wide sync at all
([containers/skopeo#364](https://github.com/containers/skopeo/issues/364), open six years).
OCX enumerates from `c/index.json`, a static file: no auth-scope negotiation, no rate limit on
discovery, and the list contains exactly the packages someone deliberately published.
[`research_registry_mirror_tooling.md`](./research_registry_mirror_tooling.md) §1.

### Reconciliation: the superseded ADR

`external/ocx/.claude/artifacts/adr_oci_registry_mirror.md` still reads **Status: Accepted** with
**Superseded By: N/A**. The owner has verbally deprecated it; the file does not say so, and
`adr_servable_index_snapshot.md:691` already prescribes setting its `Superseded By` for the
index-tree half. Recording the full picture here, since the reconciliation now spans three ADRs:

| Half of `adr_oci_registry_mirror.md` | Disposition |
|---|---|
| Index-tree half | Superseded by `adr_servable_index_snapshot.md` (already prescribed at `:691`; the vendored copy does not yet carry the amendment) |
| Client-declared `[mirrors]` transport rewrite | **Shipped and live.** Not superseded — it remains the right answer for a proxying corporate registry |
| Content-copy half (the mirror as a *replacement* rather than a proxy) | **Superseded by this ADR** |

**R1–R6 survive as constraints on this design**, not as history:

| # | Original requirement | How this ADR honours it |
|---|---|---|
| R1 | Destination is host + repository-path prefix (Artifactory repo-key method) | `target.repository` is a **prefix**; `destination:` template expands beneath it |
| R2 | Replace semantics — never contact the upstream origin | The consumer's index names only the corporate registry. There is no fallback to fall back to |
| R3 | Per-source mapping | One `sources[]` entry per upstream index, each with its own `as:` and filters |
| R4 | `ocx.lock` portable across mirrored and direct-egress hosts | **Verified.** The lock records logical identity only; the rewrite lives exclusively in index documents ([`research_mirror_lock_portability.md`](./research_mirror_lock_portability.md), verdict) |
| R5 | Mirror needs its own credentials | `OCX_AUTH_<dest-slug>_*`, resolved by ocx. Zero credentials in the spec |
| R6 | Tamper resistance | Digest-pinned transfer, and **nothing more** — see *Trust scope*, where the residual is named rather than claimed away |

---

## Decision Drivers

- **Air-gap correctness is binary.** A mirror that copies 99% of what a consumer needs is a mirror
  that does not work.
- **The published index tree is an external contract.** Once an operator points
  `[registries."ocx.sh"] index = "https://pages.corp/ocx.sh"` at it, its layout is load-bearing for
  every machine in the fleet. One-way door.
- **Reuse the shipped index machinery, do not re-implement it.** `serialize_root`, `IndexStore`,
  `regenerate_catalog` exist and are byte-parity-tested. A second implementation of a frozen wire
  format is Block-tier under `quality-core.md`.
- **Do not duplicate the OCI transport layer.** This repo has deliberately never owned one.
- **Blast radius honesty.** The mirror becomes the fleet's trust root. Say so in writing rather than
  implying that digest-pinning covers it.

---

## Industry Context & Research

**Research artifacts** (all 2026-08-13):
[`research_registry_mirror_tooling.md`](./research_registry_mirror_tooling.md) ·
[`research_mirror_supply_chain.md`](./research_mirror_supply_chain.md) ·
[`research_mirror_lock_portability.md`](./research_mirror_lock_portability.md) ·
[`research_mirror_operability.md`](./research_mirror_operability.md)

**Key insights driving the decision:**

1. **Glob for names, regex for tags** is where the ecosystem converged (Harbor, Artifactory, zot).
   Client tools that chose regex for names predate the convention. This design has no tag filter at
   all in v1, so glob-only means **no regex anywhere in the schema**.
2. **Exclude-is-a-veto must be stated, not inherited.** Artifactory publishes it cleanly; regsync's
   published documentation contradicts itself on exactly this point (tooling research §2).
3. **Destination collision is the universal footgun.** skopeo flattens to basename and silently
   overwrites; Harbor's flatten-N-levels is the same defect from the other side; **collision policy is
   undocumented by all seven surveyed products** (§3). Mandating `{registry}` makes it structurally
   unreachable — a validation rule no competitor has.
4. **Append-only is the industry default**, not a limitation (§4): Harbor never propagates deletion
   for scheduled rules, zot has not implemented it, Quay is archival by design.
5. **Referrers do not travel by default and the failure is silent** — Harbor
   [#23210](https://github.com/goharbor/harbor/issues/23210) is exactly this bug (supply-chain research §3b).
6. **A rewriting mirror is the 2008 Cappos attack surface.** Digest-pinning secures a transfer, not a
   claim (supply-chain research, *The crux*).

---

## Considered Options — the overall shape

### Option A: Index-only mirror (`ocx index sync` + static file server)

Ship nothing. Tell operators to run the shipped ocx verb and serve the tree.

| Pros | Cons |
|------|------|
| Zero new code, zero new spec, zero new trust root | **Does not close the air gap** — every `repository` still names `ghcr.io` |
| Already parity-tested and released | Useless for the motivating deployment |

### Option B: Transparent client-side `[mirrors]` redirect (the superseded ADR)

Consumers set `[mirrors]` so OCI reads route to the corporate registry's proxy repositories.

| Pros | Cons |
|------|------|
| Shipped; no mirror-side run at all | Needs a **proxying** registry with egress — the air-gapped case has none |
| Consumers keep the public index verbatim | Corporate registry must front every upstream host separately |
| No new trust root | Cold-cache first pull fails closed inside the perimeter |

### Option C: Full copy with `repository` rewrite (**chosen**)

Copy OCI content by digest into the corporate registry; rewrite each root's `repository`; write the
resulting index tree to a directory the operator serves.

| Pros | Cons |
|------|------|
| The only option that actually works air-gapped | The published tree becomes the fleet's trust root (residual, named below) |
| Enumerates from a static catalog, not `_catalog` | Bytes are real: low hundreds of GB for the full public catalog |
| Real atomic-visibility guarantee (root written last) | Needs a blob-copy seam this repo does not have today |
| `ocx.lock` stays byte-identically portable (R4) | New spec schema = new external contract |

### Option D: Shell out to `oras cp --recursive` per package, own only the index tree

Let ORAS do the content copy (it walks the referrers graph natively); ocx-mirror writes the index tree.

| Pros | Cons |
|------|------|
| Referrers-aware copy for free — the one requirement that is genuinely hard | **Credential model splits.** Settled: auth is delegated entirely to ocx (`OCX_AUTH_<slug>_*` → docker store → anonymous). ORAS reads only the docker credential store, so an `OCX_AUTH_*`-configured destination silently fails |
| No transport code in this repo | New binary dependency in `ocx.toml` for every mirror repo and every CI runner |
| Battle-tested copy semantics | oras-go carries the same credential-forwarding CVE class ([GHSA-jxpm-75mh-9fp7](https://github.com/oras-project/oras-go/security/advisories/GHSA-jxpm-75mh-9fp7)) — not a security win |
| | Error classification becomes exit-code archaeology over a foreign tool's stderr |

### Weighted evaluation

| Criterion | Weight | A | B | **C** | D |
|---|---|---|---|---|---|
| Closes the air gap | 5 | 1 | 1 | **5** | 5 |
| No credential-model split; one auth path | 4 | 5 | 5 | **5** | 1 |
| Reuses shipped index machinery | 3 | 5 | 5 | **5** | 4 |
| Bounded new maintenance surface | 3 | 5 | 5 | **3** | 3 |
| No new trust root introduced | 3 | 5 | 5 | **1** | 1 |
| Operator steps to a working mirror | 2 | 5 | 3 | **4** | 3 |
| Reversibility (pre-1.0 verb, additive spec) | 2 | 5 | 5 | **3** | 3 |
| **Total** | | **75** | **69** | **86** | **65** |

**Chosen: Option C.** A and B score well precisely because they do nothing new — and they score 1 on
the criterion that is the reason the feature exists. D loses on the credential split, which is not a
detail: it would put a second, weaker auth path in front of the corporate destination, and the
settled auth doctrine exists to prevent exactly that.

**Reversibility note.** Option C's 3/5 is honest. The verb and spec schema are pre-1.0 and additive;
the **published tree layout** is not — once a fleet's `[registries]` names `<output>/<as>`, moving it
is a coordinated fleet change. That is why `output:` is a parent home with one subtree per source
(the tree's address *is* the registry's identity) rather than a template with segments inside it.

---

## Decision Outcome

**Chosen Option: C — full copy with `repository` rewrite, published as a static index tree.**

### Settled with the owner (recorded, not re-litigated)

1. **Separate `RegistrySpec` in its own `registry.yml`.** Not folded into `MirrorSpec`. They share
   `target` and `concurrency` only. A `kind:` discriminator on both documents gives a clear error for
   a misplaced file — `MirrorSpec` carries `#[serde(deny_unknown_fields)]` (`src/spec.rs:66-68`), so
   without a discriminator a misplaced `registry.yml` reports `unknown field 'sources'` and buries the
   real problem.
2. **One verb:** `ocx-mirror registry sync [SPEC]`, optional positional defaulting to `./registry.yml`.
   Asymmetric with `package sync <SPEC>` on purpose — a package mirror repo holds many specs, a
   corporate registry mirror repo holds exactly one.
3. **`output:` is a parent index home**, one subtree per source named by `as:`. Not a template.
4. **Destination template `{registry}/{namespace}/{package}`**, plain string substitution, no template
   engine. Derived from the **package name** (the catalog key), never from the physical `repository`.
   `{registry}` mandatory when more than one source is configured. Output lowercased; case-collisions
   refused at plan time.
5. **`as:` per source**, defaulting to `registry:` verbatim. **Hard error — never silent
   slugification — when the value is not a legal OCI path component.** It doubles as the output
   subdirectory and the `{registry}` expansion.
6. **Glob include/exclude over the two-segment package name** (`*`, `{a,b}`; no `**`, no `?`).
   Exclude is a veto: a package passes iff it matches some include **and** no exclude. Empty include
   means everything. No regex anywhere in the schema.
7. **No version filter in v1.** A verbatim root copy carries the rolling-alias cascade for free; a
   version subset makes `latest` resolve to a digest nothing copied. Version windows are v2 and need
   `ocx package cascade repair`.
8. **The root document is written only after the blob push succeeds.** The index write is the atomic
   visibility point.
9. **Blob dedup:** unconditional HEAD against the destination repository, plus an in-run
   `digest → repository already written` map used for opportunistic cross-repository mounts. A mount
   is an optimistic attempt — any non-`201` outcome falls through to an ordinary upload.
   `target.blob_anchor` is opt-in and absent by default.
10. **Auth is delegated entirely to ocx** (`OCX_AUTH_<slug>_{TYPE,USER,TOKEN}` → docker credential
    store → anonymous). **Zero credentials in the spec**; credential-shaped fields refused at load
    with exit 64, the same doctrine as `policy_check_notify` (`src/spec/validate.rs`). *See the defect
    note under Consequences — the stated mechanism does not work as-is.*
11. **No GitLab/CI renderer, no merge-request creation.** The mirror writes files and stops;
    `git add/commit/push` is the operator's two lines.
12. **The corporate mirror never announces.** It owns its index and writes the tree directly.

### Sub-decision: the destination reads `target.repository` as a *prefix*

`Target` (`src/spec/target.rs:6-10`) is shared verbatim, but `repository` means something different
here: in `MirrorSpec` it is the full destination repository, in `RegistrySpec` it is the Artifactory
repo-key prefix that `destination:` expands beneath. `Target::reference()` (`:20-23`) still composes
correctly because it only joins with `/`. Recorded because a shared type read two ways is exactly the
kind of thing a later reader re-derives wrongly.

---

## Open question 1 (resolved): where the blob-copy seam lives

**Two corrections to the framing, both verified in the trees.**

**Correction 1 — the primitives already exist, and they are `pub`.** `OciTransport`
(`crates/ocx_lib/src/oci/client/transport.rs:49`) already declares `head_blob` (`:99`),
`push_manifest_raw` (`:166`), `push_blob` (`:178`) and `mount_blob` (`:196`), and the trait itself is
re-exported publicly (`client.rs:116`). What is missing is a **route to an instance**: `Client`'s
transport field is private (`client.rs:151`), `Client::with_transport` is `#[cfg(test)]` (`:182-183`),
and `native_transport` is `pub(crate)` (`:105`). The read half is *already* public —
`Client::head_blob` (`:600`) and `Client::pull_blob` (`:652`). So the upstream diff is **three or four
thin `Client` wrappers over methods that already exist and are already exercised by
`push_multi_layer_manifest`**, not a new transport layer.

**Correction 2 — referrers are further along than assumed.** The vendored fork already implements the
referrers API *with* the fallback-tag schema: `pull_referrers` at
`external/rust-oci-client/src/client.rs:2002`, documented at `:1981-2001`. `ocx_lib` does not surface
it (no `referrers` symbol anywhere in `crates/ocx_lib/src`), so the referrers-aware copy is a
**wrapper** exercise, not a protocol implementation.

### Options

**Seam 1 — thin `Client` wrappers upstream in `ocx_lib`** (submodule change + upstream PR).
Four methods: `copy_blob_from` / `blob_exists` / `push_manifest_bytes` / `list_referrers`, plus making
`fetch_manifest_raw_bytes` (`client.rs:1886`, currently `pub(crate)`) public — the raw bytes are
mandatory, because a typed manifest round-trip re-serializes and changes the digest.

**Seam 2 — drive the vendored `oci-client` fork directly from ocx-mirror.**

**Seam 3 — shell out to `ocx`.** There is no `ocx` verb that copies blobs across registries, so this
is Seam 1 plus a subprocess boundary.

**Seam 4 — shell out to `oras cp --recursive`.** Option D above, at the seam level.

| Criterion | W | **Seam 1** | Seam 2 | Seam 3 | Seam 4 |
|---|---|---|---|---|---|
| One auth path (settled decision 10) | 5 | **5** | 2 | 5 | 1 |
| Credential/SSRF policy stays in one place (ocx#272) | 5 | **5** | 1 | 5 | 2 |
| Referrers copy reachable | 4 | **4** | 4 | 4 | 5 |
| No duplicated transport layer in this repo | 4 | **5** | 1 | 5 | 5 |
| Error classification into `MirrorError`/`ExitCode` | 3 | **5** | 4 | 2 | 1 |
| No new external tool dependency | 3 | **5** | 5 | 5 | 1 |
| Independence from upstream release cadence | 2 | **1** | 5 | 1 | 5 |
| **Total** | | **106** | **60** | **97** | **62** |

**Recommendation: Seam 1.** The submodule bump to ≥0.5.8 is a hard prerequisite anyway, so the
upstream dependency is already on the critical path and costs nothing extra. Seam 1 keeps auth,
redirect/SSRF handling, retry and error classification in the single place this project has always
kept them — which is the whole reason ocx-mirror has never owned a transport. Seam 3's only loss
against Seam 1 is error classification through a subprocess, and it buys nothing Seam 1 does not
already have.

**Risk on Seam 1, and its mitigation.** It couples this feature to an upstream merge. Mitigation: the
four wrappers are additive, mechanical, and each has an existing in-tree caller pattern to copy; they
can land in the submodule ahead of everything else in this ADR and be verified in isolation.

**Named consequence — memory, not a detail.** `OciTransport::push_blob` takes `Vec<u8>` (`:178`) and
`pull_blob` returns `Vec<u8>` (`:91`). There is no streaming blob-to-blob path in `ocx_lib` today.
Peak resident memory is therefore `blob_concurrency × largest_blob`. The largest sampled catalog
asset is bazel's `dist.zip` at 221 MB (operability research §6), so `buffer_unordered(8)` would peak
near 1.8 GB. **Default `concurrency.max_downloads` to 4 for `registry sync`** and document the
ceiling. A streaming copy (`pull_blob_to_file` exists at `transport.rs:94`; a streaming `push_blob`
does not) is deferred — trigger: a catalog blob exceeds ~500 MB, or a real run is observed OOMing.

---

## Open question 2 (resolved): pruning

**Append-only forever in v1. No `registry prune` verb.**

This lands on the industry default rather than choosing a side: Harbor never propagates deletion for
manual or scheduled rules, Artifactory's is opt-in, zot's is unimplemented
([project-zot/zot#3102](https://github.com/project-zot/zot/issues/3102)), Quay is archival by design
(tooling research §4). It is also forced by ocx's own doctrine — `adr_servable_index_snapshot.md`
encodes *"No delete verb on `IndexStore`"* as a ratified constraint.

The consequence to state plainly: **narrowing an `exclude:` glob does not remove anything already
copied.** The repair is to delete the output subtree and re-run, which re-copies from scratch; the
registry-side blobs stay. If a `registry prune` verb is ever built it is an explicit, opt-in,
separately-designed verb — not a flag on `sync`.

---

## Open question 3 (RESOLVED — owner ruling, 2026-08-14): trust scope for v1

**Ruling: integrity first. This mirror carries integrity, not trust, and says so.**

Trust — authenticity and provenance, as opposed to "these bytes are the bytes the digest names" —
arrives with **signing over the OCI referrers API**. That work is in active development in `ocx` and
is **not ready** (milestone *Signing & Trust v1*: [ocx#195](https://github.com/ocx-sh/ocx/issues/195)
referrers-capable acceptance registry, [ocx#196](https://github.com/ocx-sh/ocx/issues/196)
offline/air-gapped verify). Until it lands there is nothing signed for a mirror to preserve, and a
bespoke trust mechanism built here would be a second, competing answer that signing would then have
to displace. **So v1 mirrors content and index faithfully, claims integrity only, and a future
revision mirrors referrers alongside them** — at which point the trust story is signing's, not this
document's ([ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7)).

**The one thing this defers that must not be deferred silently.** Referrer copy is safe to omit
*only while nothing upstream is signed*. The moment a source publishes a signed artifact, a copy that
walks only tagged manifests drops that signature with no error and no diagnostic — Harbor's
[#23210](https://github.com/goharbor/harbor/issues/23210) is that exact failure. So v1 does not copy
referrers, but it **must detect them**: after copying a manifest, query
`/v2/<name>/referrers/<digest>` (the vendored fork already implements this, including the
`sha256-<digest>` fallback-tag schema) and, when the response is non-empty, **fail the package with a
counted error naming what was not copied**. That converts "signatures vanished" from a silent future
regression into the trigger that says referrer mirroring is now required. Cheap now, and it is the
difference between a deferral and a latent defect.

**Superseded recommendation, retained for the record:** accept the residual for v1 in writing with the
threat named, and do not scope a signed freshness manifest into this ADR. The ruling above supersedes
the framing but keeps both conclusions — no freshness manifest here, and the residual stated plainly.

The threat, stated without hedging: **the mirror's git repository and static host are the sole root of
trust for every consumer pointing `index=` at them.** Anyone able to write there — compromised CI, a
leaked deploy credential, a malicious insider — can roll back any package to an older genuinely-signed
digest, freeze the mirror indefinitely while it looks current, or point any tag at a digest they
control, undetectably, for the entire fleet. Digest-pinning does not touch any of this: it proves
bytes were not corrupted in one transfer, and says nothing about whether the digest shown is current
or ever existed as published. This is the gap Cappos et al. broke in 2008 and TUF exists to close
(supply-chain research §1).

**Why not fix it here.** The cheapest credible fix is a small signed manifest at the tree root
carrying `sequence` / `published_at` / `valid_until`. Writing it is easy; **enforcing it requires a
consumer-side change in `ocx`, and unenforced metadata is decoration.** That makes it a follow-on ADR
with an ocx dependency, not a v1 mirror feature. PEP 458 is the precedent for retrofitting exactly
this onto a shipping static index without a breaking change.

**What v1 must not claim.** That digest-preservation gives the mirror supply-chain integrity.
"Digests are byte-identical" must not read as "trustworthy" in the docs or in any security review.

---

## Consequences

### Two defects found in the settled decisions — flagged, not silently redesigned

**Defect 1 — the exit-64 credential-rejection mechanism does not work as specified.**
Settled decision 10 says credential-shaped fields are refused at load with exit 64, *"same doctrine as
`policy_check_notify`"*. That doctrine works for `notify` because `webhook_secret` **is a schema
field** whose *value* is policy-checked after deserialization (`src/spec/load.rs:64-69` runs the policy
check before `validate`). A credential-shaped field in `registry.yml` is by construction **not** in
the schema, so `#[serde(deny_unknown_fields)]` rejects it during deserialization and produces
`SpecInvalid` → **exit 65**, with the message `unknown field 'password'`. The policy check never runs.

**Fix (cheap, and it belongs in the design):** before deserializing, scan the raw
`serde_yaml_ng::Value` mapping at any depth for a fixed deny-list of credential-shaped keys
(`password`, `token`, `username`, `auth`, `credentials`, `secret`, `api_key`) and return
`SpecUsageError` → 64 with a message naming the environment variable to use instead. The pre-scan
runs before `serde_yaml_ng::from_value`, mirroring the ordering comment already at
`src/spec/load.rs:64-66`.

**Defect 2 — the catalog-digest short-circuit does not survive the rewrite.**
The operability research's headline result — *"a no-op sync of N packages costs exactly 1 HTTP
request"* (§3) — assumes the local roots are byte-identical to the source's, so their digests can be
compared against the source catalog's `packages[id] = sha256(root_raw)`. **This mirror rewrites
`repository`, so every local root hashes differently by construction.** The short-circuit as written
compares two things that can never be equal and would report every package as changed on every run.

Two recoveries, and the design should take the first with the second as automatic fallback:

- **(a) Cache the source catalog** verbatim at `<output>/.ocx-mirror/<as>/source-catalog.json`, written
  only after a fully successful run. Next run: one GET, compare against the cache, O(1) preserved.
  This is **not** the resume journal decision 8 rejected — losing or staling it costs one extra
  comparison pass, never correctness, and an interrupted run simply leaves it stale.
- **(b) Fallback when the cache is absent or unreadable:** fetch each filtered package's source root
  and compare `tags{}` against the local root. O(N) small JSON GETs, zero bytes, zero state — ~227
  requests at full public-catalog scale, which is seconds.

Recording both is what keeps a later reader from re-deriving (a) as a journal and rejecting it.

### Positive

- The only design in the surveyed field that enumerates from a published static catalog instead of
  `_catalog`, states its destination-collision policy **and enforces it structurally**, and offers a
  real atomic-visibility boundary (tooling research §10).
- `ocx.lock` is byte-identically portable between a mirrored host and a direct-egress host — verified
  against the lock schema, not assumed (`lock.rs:170`, `resolve.rs:516`).
- Resumability falls out of ordering discipline already required for correctness. The output directory
  is the checkpoint: on interrupt at most one package has blobs copied and no root written.
- Reuses `serialize_root` / `serialize_catalog` / `serialize_config` — the byte-parity-tested writers —
  so the tree cannot drift from what ocx and `indexbot render` produce.

### Negative / accepted

- **The mirror becomes the fleet's trust root, unbounded** (open question 3).
- **Append-only forever**: a narrowed filter does not un-copy anything.
- **Peak memory is `concurrency × largest blob`** until a streaming copy exists.
- **A mirrored-only host cannot resolve a package the mirror never copied**, even though the same
  package resolves fine publicly. True of any curated mirror; worth saying because the failure reads
  as "package does not exist".
- **`__ocx.desc` needs explicit handling** — readme and logo live at `<repository>:__ocx.desc`, and
  that tag is classified administrative, so *any* `root.tags{}` walk skips it automatically
  (lock-portability research). Not an install-path break (ocx models no `desc` on the install path);
  it breaks catalog/website rendering only.
- **Never be first mover on `format_version`.** An unrecognized version is a hard 65 at every reader
  (`wire.rs:49`, `:68`) — flag-day by design.
- **Overlapping runs are the operator's problem, not the tool's.** Harbor ships a *single active
  replication* toggle for exactly this, and this repo already enforces the equivalent invariant on the
  surface it owns — the generated workflows' `concurrency:` block with `cancel-in-progress: false`
  ([`subsystem-mirror.md`](../rules/subsystem-mirror.md) § R1). Settled decision 11 means
  `registry sync` renders no CI, so it cannot emit that block. `CatalogTransaction` serialises the
  catalog write, but two concurrent runs against one output tree still race on root documents.
  **Document it as a required `concurrency:` group on the operator's own job**; do not add a lockfile
  to a tree whose whole point is being a clean servable artifact.

### Risks

- *Referrers dropped silently.* If the copy walks only `root.tags{}`, every cosign signature and
  attestation vanishes with no error and the copy reports success — Harbor
  [#23210](https://github.com/goharbor/harbor/issues/23210) verbatim. Blocks
  [ocx-sh/ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7). **Mitigation:** referrers walk
  is in scope for v1, using the fork's existing `pull_referrers` (fallback-tag schema included).
- *Credential forwarding across a host boundary on the push path.* [GHSA-jxpm-75mh-9fp7](https://github.com/oras-project/oras-go/security/advisories/GHSA-jxpm-75mh-9fp7)
  (CVSS 7.5, CWE-918) is the same defect class in the sibling implementation, tracked here as
  [ocx-sh/ocx#272](https://github.com/ocx-sh/ocx/issues/272). This ADR's push path is the first
  ocx-mirror code to open blob upload sessions against a registry the operator does not control the
  redirect behaviour of. **Mitigation:** Seam 1 keeps this in `ocx_lib`, where the fix lands once.
- *A tree written without `config.json`.* An ocx < 0.5.8 reading it fails **silently** on every
  package — `NotAnIndex` → `resolve_root` returns `Ok(None)`. **Mitigation:** always write it (below).
- *Artifactory repository-type mismatch.* The destination must be an **OCI-type** repository (7.74+),
  not a legacy Docker-type repo-key; the latter answers `unable to upload blob … unknown: Not Found`.
  Referrers need 7.90.1+ and are scoped to OCI/Helm-OCI types only. **Mitigation:** document as a
  prerequisite; choosing OCI-type now avoids a later repo migration.

---

## Technical Details

### Architecture

```
registry.yml ──► RegistrySpec (validated at load)
                     │
   per source ───────┤
                     ▼
        ┌─ GET <index>/config.json      ── gate format_version, keep raw bytes
        ├─ GET <index>/c/index.json     ── enumerate (static file; no _catalog)
        │        │
        │        ├── filter: include ∧ ¬exclude over "<ns>/<pkg>"
        │        └── short-circuit vs cached source catalog  (else per-root compare)
        │
        └─ per changed package:
             GET  p/<ns>/<pkg>.json                     ── raw bytes + typed root
             for each tags{}.content digest, and __ocx.desc:
                 pull manifest (raw bytes, by digest)
                 for each referenced blob + child manifest + referrer:
                     HEAD dest  ──hit──► skip
                          └─miss─► try mount (in-run digest→repo map / blob_anchor)
                                     └─ non-201 ─► pull source blob ─► push dest
                 push manifest bytes verbatim (digest preserved)
             ── all content confirmed at destination ────────────────► THEN:
             parse root bytes → serde_json::Value
             mutate exactly one key: "repository"
             serialize_root(&Value) → write via CatalogTransaction::write_root
                                       (catalog entry = sha256 of the REWRITTEN root)
        commit transaction ─► c/index.json ; write config.json ; cache source catalog
```

### `RegistrySpec` — the wire contract

```yaml
kind: registry                       # discriminator; MirrorSpec carries `kind: package`

target:                              # crate::spec::Target, shared verbatim
  registry: artifactory.corp.example
  repository: ocx-mirror             # PREFIX (Artifactory repo-key), not a full repository
  blob_anchor: ocx-mirror/_blobs     # OPTIONAL, absent by default; one NAMED source repository
                                     # for cross-repo mounts (the ECR shape, not a feature flag)

output: ./public                     # PARENT index home; one subtree per source

destination: "{registry}/{namespace}/{package}"   # plain substitution; no template engine
                                                  # {registry} REQUIRED when sources.len() > 1

sources:
  - registry: ocx.sh                 # logical registry name — the `<ns>` consumers configure
    index: https://index.ocx.sh      # where that registry's index tree is served
    as: ocx.sh                       # OPTIONAL; default = `registry` verbatim.
                                     # Output subdir AND the {registry} expansion.
                                     # Hard error if not a legal OCI path component.
    include: ["kubernetes/*", "{hashicorp,ocx}/*"]   # empty ⇒ everything
    exclude: ["*/internal-*"]                        # veto: match ⇒ excluded, always

concurrency:                         # crate::spec::ConcurrencyConfig, shared verbatim
  max_downloads: 4                   # blob-copy semaphore; 4 not 8 — see the memory ceiling
  max_retries: 3                     # reactive 429 / Retry-After backoff (push_with_retry shape)
```

**Validation rules, all at load, all exit 64/65:**

| Rule | Error | Exit |
|---|---|---|
| Credential-shaped key anywhere in the document | `SpecUsageError` (pre-scan, see Defect 1) | 64 |
| `kind:` absent or not `registry` | `SpecUsageError` | 64 |
| `as:` not `[a-z0-9]+(?:[._-][a-z0-9]+)*` per segment | `SpecInvalid` | 65 |
| Duplicate `as:` across sources | `SpecInvalid` | 65 |
| `{registry}` missing from `destination` with >1 source | `SpecInvalid` | 65 |
| Unknown placeholder in `destination` | `SpecInvalid` | 65 |
| Glob containing `**` or `?` | `SpecInvalid` | 65 |
| Two packages expanding to the same lowercased destination | `SpecInvalid` (at plan time) | 65 |

`ocx.sh` and `ghcr.io` pass the `as:` grammar (dots are legal per the distribution-spec name grammar);
`localhost:5001` does not, and gets a hard error naming `as:` as the fix.

### CLI surface

```
ocx-mirror registry sync [SPEC] [--dry-run] [--fail-fast]

  SPEC          Path to the registry spec  [default: ./registry.yml]
  --dry-run     Report what would be copied — package counts AND estimated bytes — and copy nothing
  --fail-fast   Abort on the first package failure (default: continue-on-error, fail at end)
```

`--fail-fast` reuses the flag already on `SyncOptions` (`src/command/package/options.rs`). Default is
**continue-on-error / fail-at-end** (skopeo's `--keep-going` semantics): one broken package must not
abort 226 healthy ones, and the process exits non-zero iff anything failed. Confirmed as Harbor's
real behaviour too, read from its source rather than its docs — an `Execution` carries live
`Total` / `Succeed` / `Failed` / `InProgress` / `Stopped` counters with `Task` at per-artifact grain.

**Summary line, borrowing that counter set:** `N total, M copied, K skipped, J failed`. A no-op run is
**never silent** — it prints the same line with `0 copied`, because silence is indistinguishable from
"did not run" in a CI log.

`registry validate` is deliberately **not** added. `--dry-run` parses and validates. Trigger to
reconsider: an operator wants a network-free pre-commit gate.

### The copy engine seam — concrete functions, no trait

`src/registry/copy.rs` holds free `async fn`s over `&Publisher`. **No `BlobSink` trait.** There would
be exactly one production implementation, and the test seam already exists: the acceptance harness
runs a real Docker registry on `:5001` (`test/`), which is a better oracle for digest preservation and
mount fallback than any in-process double. `quality-core.md` YAGNI: extract an abstraction when a
second genuinely different implementation appears.

Required from `ocx_lib` (Seam 1 upstream additions, all thin `Client` wrappers):

| Needed | Status today |
|---|---|
| `Client::head_blob(&Identifier, &Digest) -> Result<u64>` | **Already public** — `client.rs:600` |
| `Client::pull_blob(&PinnedIdentifier) -> Result<Vec<u8>>` | **Already public** — `client.rs:652` |
| raw manifest bytes by digest | `fetch_manifest_raw_bytes` is `pub(crate)` — `client.rs:1886`. **Expose.** A typed round-trip re-serializes and changes the digest, so raw bytes are mandatory |
| push blob by digest | `OciTransport::push_blob` — `transport.rs:178`. **Wrap.** |
| push manifest bytes verbatim | `OciTransport::push_manifest_raw` — `transport.rs:166`. **Wrap.** |
| cross-repo mount | `OciTransport::mount_blob` — `transport.rs:196`, `MountOutcome` already `pub` (`client.rs:116`). **Wrap.** |
| list referrers | fork has `pull_referrers` with fallback-tag schema — `external/rust-oci-client/src/client.rs:2002`. **Surface through the transport.** |

### Destination skip check — reuse, do not reinvent

Copy-what-is-missing is settled ecosystem practice, not an invention — regsync spells it
`once --missing`, and pairs `fastCopy` with `forceRecursive` for the same skip-versus-rewalk choice
this design makes between the HEAD skip and the referrers walk.

`src/pipeline/target_registry.rs` already solves this exact problem and its module doctrine (`:1-13`)
is the rule to follow: **only an authoritative registry not-found may classify content as absent;
every other error aborts.** `ClientError::BlobNotFound` / `RepositoryNotFound` /`ManifestNotFound` are
distinct variants for precisely this (`oci/client/error.rs:69`, `:76`, `:85`). A HEAD that fails with a
503 must **not** be read as "blob absent and therefore needs uploading" — that is the mirror-side twin
of issue #157, and it would re-upload the whole catalog on a flaky link.

### Index-store construction — `locks_root` must be redirected, and this is not a footnote

```rust
// output: ./public   as: ocx.sh   ⇒   ./public/ocx.sh/{config.json,c/,p/}
let store = IndexStore::new(&spec.output)          // root = the PARENT index home
    .with_locks_root(&run_locks_dir);              // MANDATORY — see below
let mut transaction = store.begin_catalog_transaction(&source.as_name).await?;
```

`IndexStore::new` defaults `locks_root` to `root.join("locks")` (`index_store.rs:62-66`). Left
alone, `registry sync` would create a `locks/` directory **inside the very tree the operator serves
statically and commits to git**. Both real upstream construction sites already redirect it
(`regenerate.rs:283-285`), and the doc comment on `with_locks_root` (`:68-72`) says why in as many
words: *"Locks must never land inside a redirected … or shipped index home."*

**Which layout, and why it is not reopened.** This is the **wrapped** case —
`IndexStore::new("./public")` with `source = "<as>"` — which is the store's ordinary shape and needs
no special-casing. `adr_servable_index_snapshot.md` OQ1 (root the store one level up, pass the
checkout directory name as `source`) addresses the *unwrapped* layout, where the tree root *is* the
served root. Settled decision 3 chose the wrapped layout, so OQ1 does not apply here.

**Constraint on how the tree is written: no symlinks under `p/`.** `list_wire_repositories` branches
`is_dir()` then `!is_file()` (`index_store.rs:772-782`), and a symlink is neither — so a symlinked
root is skipped and a symlinked *directory* takes **every root beneath it** in one step. Because
`regenerate_catalog` replaces the catalog wholesale, that is not one missing entry but silent bulk
removal (`regenerate.rs:50-61`). The mirror writes real files and real directories only.

**One inherited side effect worth knowing before running against a git checkout:**
`CatalogTransaction::commit` unconditionally `remove_file`s `c/index.json.etag` *before* its
unchanged-catalog early return, failure ignored on purpose (`regenerate.rs:42-46`). If an output tree
ever tracks that file in version control, a sync deletes it and cannot fail on it. Otherwise `commit`
writes nothing when the catalog is unchanged (`index_store.rs:1156-1158`), so a no-op run leaves the
tree byte- and mtime-identical — which is what makes it safe to run on a schedule against a
committed tree.

### Index-tree writing — every document, and who writes it

| Document | Action | Writer |
|---|---|---|
| `p/<ns>/<pkg>.json` | Parse raw bytes to `serde_json::Value`, mutate **exactly** the `repository` key, re-serialize. `IndexRoot` is `Deserialize`-only (`wire.rs:144-159`) and has no `deny_unknown_fields` for fleet forward-compat, so a *typed* round-trip would silently drop newer fields the mirror must pass through | `wire_writer::serialize_root(&Value)` (`:59`) via `CatalogTransaction::write_root` (`index_store.rs:1103`) — **not** the bare `write_root_document` (`:757`): `write_root` writes the bytes atomically *and* upserts the derived catalog entry under the one held lock, and its `repository_check` hook re-parses the rewritten bytes as `IndexRoot`, so a rewrite that produced an unparseable root fails at the write instead of shipping |
| `c/index.json` | **Derived, never copied** — ratified constraint: *"the catalog is authored, never mirrored"*. The mirror's catalog is the filtered subset, keyed on `sha256(rewritten root)` | `CatalogTransaction::commit`; `regenerate_catalog` (`regenerate.rs:121`) is the repair path |
| `config.json` | **Always written.** Fetch the source's, parse to gate `format_version` against `SUPPORTED_FORMAT_VERSION` (`wire.rs:49`), then write the **raw source bytes** verbatim. Parse-to-check, write-raw: `IndexFormatConfig` models only two keys, so a parse→`serialize_config` round trip would drop any sibling field. Source has none ⇒ write `{"format_version": 1}` | raw passthrough, or `serialize_config` (`:106`) for the synthesized case |
| `o/<algo>/<hex>.json` dispatch objects | Copied byte-for-byte; the digest pins them | `IndexStore::write_dispatch_object` (`:393`) |
| `<repository>:__ocx.desc` | **Explicitly copied** per package. Skipped by any `root.tags{}` walk because the tag is classified administrative | copy engine |

Root fields other than `repository` — `name`, `owners`, `created`, `upstream`, `desc`, and critically
`status` / `deprecated_message` / `superseded_by` — are copied verbatim and **never fabricated**. The
last three are consumed by ocx and drive yank/deprecation warnings on every resolve
(`surface_root_status`, `ocx_index.rs:851`).

**`preserve_order` is load-bearing and currently implicit.** `serialize_root` requires an
order-preserving `Value`; `ocx_lib` enables the feature (`crates/ocx_lib/Cargo.toml:50`) but
ocx-mirror's own `serde_json = "1.0.150"` (`Cargo.toml:51`) does not. It works today only through
Cargo feature unification. **Declare `features = ["preserve_order"]` explicitly** — an implicit
dependency on unification for a byte-exactness-critical path is a latent field-reordering bug.

### Error model — reuse first, two new variants

**Reused unchanged:** `SpecNotFound` (79), `SpecInvalid` (65), `SpecUsageError` (64),
`SourceError` (69, source index or source registry unreachable), `TargetError` (69, destination
registry read/write — carrying the existing fail-safe doctrine), `ExecutionFailed` (1, aggregated
per-package failures at end of run).

**New — justified individually:**

| Variant | Exit | Why not an existing variant |
|---|---|---|
| `IndexWriteError(String)` | 74 `IoError` | The output tree is a distinct failure surface with a distinct remedy (disk full, permissions). `TemplateError` is also 74 but means "workflow render failed" — reusing it would make the message lie |
| `IndexFormatUnsupported(u64)` | 65 `DataError` | A source declaring `format_version: 2` is **not** transient. `SourceError` (69) would make CI retry forever on something a retry can never fix. Follows `CascadeUnrepaired`'s precedent of a variant existing to carry a non-transient 65 through unchanged |

Both go in `src/error.rs` with a `kind_exit_code` arm (`:75-95`) and a unit test in the existing
per-variant style (`:146-218`).

### NFR coverage

| NFR | Position |
|---|---|
| **Scalability** | Full public catalog ceiling, cold: ~2,970 release-builds, ~17,800 blobs, ~390 GB, ~113,000 requests (operability §1). A real filtered corporate run is tens of packages. Enumeration is a static file — it does not scale with registry size, only with catalog size |
| **Latency** | No-op run: 1 request with the source-catalog cache, ~N small GETs without. K changed packages: 1 + K root fetches + only the blobs those roots newly reference |
| **Availability** | Continue-on-error / fail-at-end. **Reactive** 429 / `Retry-After` backoff only, reusing `push_with_retry`'s 1s-doubling-to-30s ±10% jitter shape. The distinction is load-bearing: regsync's `ratelimit` is **proactive** — a pre-flight quota check reading a remaining-pulls header before a step, not a 429 handler — and GHCR publishes no such header, so the proactive mechanism has nothing to read. Defer it until a Docker Hub source exists |
| **Security** | Zero credentials in the spec; auth entirely ocx's; `blob_anchor` is one **named source repository** (the ECR shape) not a global feature flag (the GHCR anti-pattern); credential non-forwarding across host boundaries is ocx#272, kept in `ocx_lib` by Seam 1. Trust residual: open question 3 |
| **Cost** | Bytes are the cost. `--dry-run` reports estimated bytes, not just counts — the number an operator on a metered or scheduled link needs before committing |
| **Operability** | Output tree is its own checkpoint; no journal. Non-silent no-op. Bounded blob concurrency, no KB/s cap and no chunked upload (deferred with triggers from operability §6) |
| **Portability** | `ocx.lock` unaffected — logical identity only. On-disk caches key on logical registry, so one machine switching between mirrored and direct config reuses one cache |

---

## Implementation Plan

1. [ ] **Bump `external/ocx` to ≥ 0.5.8**, `ocx.toml:6` (`ocx = "ocx.sh/ocx/cli:0.5.6"`) and
       `ocx.lock` in the same commit — `subsystem-mirror.md` makes the version floors an invariant
       *"enforced by keeping this repository's own `ocx.toml` / `ocx.lock` current"*, so the pin moving
       with the pointer is not optional. Fix the now-false `reqwest` sentence (below) and declare
       `serde_json` `preserve_order` explicitly while here. Blast radius in the next section.
2. [ ] **Upstream (ocx): the four `Client` wrappers** + expose `fetch_manifest_raw_bytes` + surface the
       fork's `pull_referrers` through `OciTransport`. Additive, mechanical, independently verifiable.
3. [ ] **`RegistrySpec`** in `src/spec/registry.rs` + the credential pre-scan + validation rules +
       `kind:` on `MirrorSpec`. Fixture per rejected document under `tests/fixtures/invalid/`, matching
       the existing one-file-per-rule convention.
4. [ ] **Enumerate + filter**: catalog fetch, glob engine, source-catalog cache with the per-root
       fallback, `--dry-run` reporting counts and bytes.
5. [ ] **Copy engine**: manifest/blob/referrer walk, destination HEAD skip reusing
       `target_registry.rs`'s pattern, mount attempt with upload fallback, `__ocx.desc`.
6. [ ] **Index writer**: root rewrite through `serialize_root`, `CatalogTransaction`, `config.json`,
       write-root-last ordering.
7. [ ] **Wire the verb**: `RegistryCommand` + the `Registry` arm on `Command`, deleting the placeholder
       comment at `src/command/registry/mod.rs:18-19`.
8. [ ] **Docs + rule**: `docs/`, and a `registry sync` section in
       [`subsystem-mirror.md`](../rules/subsystem-mirror.md) (module map rows, error-model rows).
9. [ ] **Amend the vendored ADR**: set `Superseded By` on
       `external/ocx/.claude/artifacts/adr_oci_registry_mirror.md` per the reconciliation table — the
       index-tree half to `adr_servable_index_snapshot.md`, the content-copy half to this ADR.

### The v0.5.6 → v0.5.8 submodule bump — measured, not assumed

**The CLI contract is low risk.** Both suspected breaking changes are **out of range** — the
`package inspect --format json` reshape (`CHANGELOG.md:192`) and the index physical-transport fix
(`:248`) both sit below the v0.5.5 boundary (`:85`), so they are already baked into the current 0.5.6
pin; ocx-mirror never invokes `ocx package inspect` in any case. The only in-range BREAKING entry is
`(lazy)`-scoped (`:17`) — tool materialization, no touchpoint with the `oci` / `package push` /
`package announce` / `index` paths this repo drives.

**Both JSON reports ocx-mirror parses were checked field-by-field and are compatible.** Push gains
`canonical_tags_written` (`ocx_cli/src/api/data/push.rs:20-36`); ocx-mirror's `PushReport`
(`src/pipeline/ocx_cli/push.rs:29-44`) reads four fields, every one `#[serde(default)]`, with no
`deny_unknown_fields` on either side — the extra field is ignored. Announce gains
`pull_request_number` / `fork` (`api/data/announce.rs:20,50,55`), same tolerant shape.
`ExitCode::TempFail = 75` is unchanged (`ocx_lib/src/cli/exit_code.rs:46`), so
`push_exit_is_transient()`'s hardcoded 75 stays correct. `[patch.crates-io]` still resolves: both
nested fork submodules exist at v0.5.8, which pins `oci-client = "0.17"` and
`docker_credential = "1.3"`.

**One real drift, and it makes a checked-in doc false.** `reqwest` is **no longer mirror-owned**: ocx
v0.5.8 re-added it directly (`Cargo.toml:100`, `reqwest = { version = "0.13", features = ["rustls"] }`,
plus a new `webpki-root-certs = "1"`) to drive `ReqwestIndexTransport`. ocx-mirror is still on
`reqwest` **0.12** with different features (`Cargo.toml:68`). So the sentence carried in both
`Cargo.toml:42-45` and `CLAUDE.md` § Dependency model — *"Since v0.4.1 `reqwest`, `rustls`,
`octocrab`, `url` are mirror-owned — ocx dropped them, so there is no upstream source of truth"* — is
now **wrong for `reqwest`**. `octocrab` and `url` remain absent upstream. `rustls` is no longer a bare
top-level ocx dependency at all (only a reqwest feature), while ocx-mirror pins it explicitly
(`Cargo.toml:69`) because `main.rs:31-33` installs the aws-lc-rs provider directly.

Two semver-incompatible `reqwest` majors coexist in one lockfile without error, so this is **not a
hard blocker** — but the bump commit must carry **(a)** the corrected wording in both places, and
**(b)** a `cargo tree -i reqwest -i rustls` verification, which nobody has run yet. Getting two TLS
stacks in one binary through inattention is exactly what that comment block exists to prevent.

### Migration / rollout — what existing consumers see

| Population | What happens |
|---|---|
| Existing `mirror.yml` specs | **Nothing.** `kind:` is optional-with-default on `MirrorSpec`; an existing spec parses unchanged |
| Existing `package` verbs and generated workflows | **Nothing.** No shared code path is modified; `Target` and `ConcurrencyConfig` are read, not changed |
| A repo that bumps the binary but not the submodule | Does not build. The submodule bump is a prerequisite, not a runtime check |
| A fleet already using `[mirrors]` against a proxying registry | **Unaffected and still supported.** This is a second, different deployment shape, not a replacement |
| A consumer pointed at a mirror tree, on ocx < 0.5.8 | Resolves correctly **because the mirror always writes `config.json`**. Without it, silent not-found on every package |
| A mirror tree copied before referrers support existed | Roots and content resolve; signatures are absent. Re-running after the referrers walk ships backfills them (HEAD-skip means only the referrers are transferred) |

**Rollout order:** steps 1–2 are independently shippable and should land first. Steps 3–6 are the
feature. Step 7 makes it reachable. Nothing before step 7 is user-visible.

---

## Validation

- [ ] A tree produced by `registry sync`, served statically, resolves every package it contains from a
      host with **no route to the public internet** (the acceptance criterion the feature exists for).
- [ ] Every copied manifest's digest at the destination equals its digest at the source — byte
      preservation, asserted on manifests and blobs alike.
- [ ] `ocx.lock` produced on a mirrored host is **byte-identical** to one produced on a direct-egress
      host for the same `ocx.toml`.
- [ ] A package carrying a cosign referrer round-trips with the referrer present at the destination —
      the Harbor #23210 regression guard.
- [ ] `__ocx.desc` is present at the destination for a package that has one.
- [ ] `config.json` is present in every written subtree; a source declaring `format_version: 2` exits
      **65** and writes nothing.
- [ ] Interrupting a run mid-package leaves no root document for that package, and a re-run completes
      it while HEAD-skipping every blob the interrupted run already pushed.
- [ ] A no-op re-run prints a non-silent report and issues exactly one catalog request (with the cache)
      — **and both outcomes are demonstrated**: the same assertion must go red on a run with one
      changed package.
- [ ] A destination HEAD failing with 503 aborts with `TargetError` (69) and does **not** re-upload.
- [ ] `registry.yml` carrying `password:` exits **64**, not 65 — the Defect 1 guard.
- [ ] Two sources whose packages expand to the same lowercased destination are refused at plan time.
- [ ] A single-source spec with no `{registry}` in `destination` is accepted; adding a second source
      makes the same spec exit 65.
- [ ] `cargo tree -i reqwest -i rustls` post-bump shows one TLS stack and no unexpected second
      `reqwest` major reaching the binary.
- [ ] No `locks/` directory and no `c/index.json.etag` exists anywhere under `output:` after a run.
- [ ] `task verify` passes.

---

## Open Questions

- ~~**[NEEDS CLARIFICATION: trust scope for v1.]**~~ **Resolved by owner ruling, 2026-08-14** — see
  Open question 3. Integrity first: trust arrives with signing over the referrers API, which is in
  active development and not ready, so v1 claims integrity only and a future revision mirrors
  referrers. No freshness manifest here. The residual stays stated plainly, and v1 **detects**
  referrers it does not copy rather than dropping them silently.

- *Raised and ruled out of scope (owner, 2026-08-14): who runs `registry sync` in CI and under what
  identity.* An actor able to write to the mirror job's output already controls the corporate registry
  and the host serving the index — there is no configuration of this tool that changes that, so it is
  not a threat this design models. Recorded so it is not re-raised: settled decision 11 leaves the job
  to the operator, and that remains deliberate.

- *Raised and ruled out of scope (owner, 2026-08-14): whether Artifactory supports cross-repository
  blob mount.* Whether a registry honours a mount is the registry operator's configuration problem,
  not this tool's, and no probe gates anything: **mount is an optimistic attempt and any non-success
  falls back to an ordinary upload** (decision 9). The one addition the ruling makes explicit — a mount
  that fails *although the source blob demonstrably exists* emits a **warning** and then uploads, so a
  misconfigured destination is visible in the run log rather than silently costing bandwidth on every
  package, forever. Nothing is gated, nothing is probed, and a registry that declines every mount
  produces a correct mirror at the cost of wire transfer only — never storage, since checksum-based
  storage deduplicates server-side regardless.

---

## Links

- [`adr_cli_namespace_restructure.md`](./adr_cli_namespace_restructure.md) — reserves this namespace (`:170`)
- [`research_registry_mirror_tooling.md`](./research_registry_mirror_tooling.md) — competitor schemas, filter grammar, collision precedent, Artifactory requirements
- [`research_mirror_supply_chain.md`](./research_mirror_supply_chain.md) — TUF taxonomy under the rewrite, referrers, credential-forwarding CVE class
- [`research_mirror_lock_portability.md`](./research_mirror_lock_portability.md) — the R4 verdict, wire-format skew, the `__ocx.desc` gap
- [`research_mirror_operability.md`](./research_mirror_operability.md) — sizing, resumability, incremental cost, rate limits, failure semantics
- ocx: `adr_servable_index_snapshot.md` — the index-tree half this builds on (decisions A, C, F)
- ocx: `adr_index_indirection.md` — wire grammar; `repository` as a routing pointer expected to be rewritten
- `external/ocx/.claude/artifacts/adr_oci_registry_mirror.md` — R1–R6; superseded per the reconciliation table
- [ocx-sh/ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7) — re-sign + attest mirrored bundles (blocked on the referrers walk)
- [ocx-sh/ocx#272](https://github.com/ocx-sh/ocx/issues/272) — credential forwarding across host boundaries
- [goharbor/harbor#23210](https://github.com/goharbor/harbor/issues/23210) — referrers silently dropped under replication

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-08-13 | architect (opus) | Submodule-bump blast radius folded into Migration: CLI contract verified low-risk (both suspected breakages predate the current pin; both parsed JSON reports gained fields tolerantly), `ocx.toml` pin bump made non-optional, and the `reqwest`-is-mirror-owned sentence in `Cargo.toml:42-45` + `CLAUDE.md` recorded as **now false** with a `cargo tree` verification step. Added the `locks_root` redirect to the component contract — `IndexStore::new` defaults it to `root/locks`, which would ship a lock directory inside the served, committed tree — plus the wrapped-layout choice (servable-ADR OQ1 does not apply), the no-symlinks-under-`p/` constraint, and `commit`'s unconditional `c/index.json.etag` removal. Kept `CatalogTransaction::write_root` over the bare `write_root_document` and recorded why. Operability: Harbor's `Execution` counter set adopted as the summary line, its single-active-replication toggle recorded as an operator responsibility this tool cannot render (decision 11), and regsync's `ratelimit` corrected to proactive — which is why reactive-only is right against a header-less GHCR. |
| 2026-08-13 | architect (opus) | Initial draft. Records the twelve settled decisions; resolves the blob-copy seam to thin `ocx_lib` wrappers (Seam 1) after finding the primitives already exist and are `pub` at the `OciTransport` level with only the *instance route* missing, and that the vendored fork already implements `pull_referrers` with the fallback-tag schema; resolves pruning to append-only; recommends accepting the trust residual in writing. Flags two defects in the settled decisions: the exit-64 credential rejection is unreachable behind `deny_unknown_fields`, and the catalog-digest short-circuit cannot survive a rewrite that changes every root's digest by construction. Names the `push_blob(Vec<u8>)` memory ceiling and drops default blob concurrency to 4. |
