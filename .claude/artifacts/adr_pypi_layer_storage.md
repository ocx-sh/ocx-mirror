# ADR: PyPI Wheel Layer-Storage Repositories (`pypi/<name>:sha256.<hash>`)

## Metadata

**Status:** Proposed
**Date:** 2026-07-18
**Deciders:** Architect (`/architect`), Michael Herwig
**GitHub Issue:** N/A (branch `feat/pypi-mirror`)
**Related Design Spec:** [`design_spec_pypi_layer_storage.md`](./design_spec_pypi_layer_storage.md)
**Related Research:** [`research_pypi_layer_storage.md`](./research_pypi_layer_storage.md)
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio) and `.claude/rules/subsystem-mirror.md`
- One deviation (direct `ocx_lib` push instead of `ocx` subprocess) justified in Q6 — additive, not a new dependency category.

**Domain Tags:** push | source | pipeline | security
**Supersedes:** the ad-hoc `register_wheel_layers` naming/push behavior on `feat/pypi-mirror` (unreleased — no back-compat obligation)

## Context

`feat/pypi-mirror` already ships a first cut of wheel-layer reuse: `register_wheel_layers`
(`crates/ocx_mirror/src/pipeline/python_push.rs`) pushes each not-yet-published wheel standalone
to `pip-packages/<index-host>/<package>:<bare-sha256-hex>` via `ocx package push` with a minimal
`Bundle` metadata, purely so the app's own env push can `:from=` cross-repo blob-mount the layer
instead of re-uploading it. This works today only because the vendored submodule sits on
`feat/layer-mount` (pre-D5), where `ocx package push` still accepts a bare `Bundle` and a
`required` `-p` flag.

This ADR formalizes that cut into a first-class **content-addressed layer-storage namespace**
and decides six open questions (Q1–Q6) that the ad-hoc cut left implicit. It is bounded by
six user-locked rulings (R1–R5) that are **not** reopened here — they are the frame, not the
subject:

- **R1** Wheel storages are pure content-addressed layer storage — **not** OCX packages (no
  entrypoints, deps, metadata contract).
- **R2** Repo path `= <registry>/pypi/<package-name>` (PEP-normalized dist name); tag `=` canonical
  `sha256.<hash>` from the PEP 751 lock's wheel hash. No version tags, no platform index storage-side.
  The lock is the resolver.
- **R3** Env packages keep **embedding** wheel layers; reuse `= :from=` server-side blob mount from
  `pypi/`, **same registry only**. Client CAS dedup + hard-link materialization unchanged — zero
  client changes.
- **R4** Wheel selection / ABI / glibc-floor stays author-side in the spec `wheels:` filter. No
  registry-side platform metadata for wheel storage.
- **R5** Alignment target `=` ocx main "unified platform model" (commit `9978cc2`): one
  `is_compatible`/`select_best` relation, lock V3 only, D5 recorded-platform binding on push/test,
  `Platform::Specific` lost `os_version` + `features`.

## Decision Drivers

- **R1 fidelity** — storages must not accrete package semantics; the shape must read as "raw blob
  under a discoverable tag", not "a package with empty metadata".
- **D5 immunity** — the chosen push path must survive the submodule bump to main, where
  `ocx package push` gains a recorded-platform equality gate and a dependency-pin gate that a
  non-package artifact cannot and should not satisfy.
- **Mount correctness** — the storage entry must be present before the env push attempts its mount
  (serial push invariant, subsystem-mirror Phase 2).
- **Registry-compatibility blast radius** — every choice is exercised against the `:5000` distribution
  fixture and must not depend on the OCI-1.1 Referrers API (uneven support).
- **Fail-safe reads (issue #157)** — only an authoritative not-found may trigger a (re)push; a
  transient registry error must never re-flag a published wheel.
- **KISS/YAGNI** — no mirror-side GC machinery, no plan-schema growth, no config knob without a
  second caller.

## Industry Context & Research

**Research artifact:** [`research_pypi_layer_storage.md`](./research_pypi_layer_storage.md)

**Trending approaches:** package-ecosystem-over-OCI proxies (PyOCI, ocipy, npm-registry-oci,
conda-oci-mirror, GitLab's roadmapped PyPI/npm virtual registries) validate the *general* direction
but all operate at **package-version** granularity. The `<algo>-<hex>` (dash) tag grammar is now
normative as the OCI **Referrers Tag Schema** fallback (cosign `sha256-….sig`, SOCI `sha-<digest>`).
The empty-config + `artifactType` + no-`subject` artifact shape is battle-tested since image-spec
v1.1.0 (Feb 2024) and is already ocx's own pattern for description/patch artifacts.

**Key insight (three, load-bearing):**
1. Wheel-*layer*-granular cross-package dedup is **novel** — no public prior art at this granularity.
   Borrow conventions, not a whole design.
2. The dash tag grammar now carries "referrer of subject X" semantics. These tags are **primary
   self-addressing CAS keys**, not referrers → deliberately diverge to `sha256.<hash>` (period) to
   avoid OCI-1.1 tooling misreading them during referrer-emulation scans.
3. Global registry-wide mark/sweep means a mounted blob survives even if the storage tag is later
   GC'd — the only real risk is a retention policy untagging **before** the first mount (a race the
   existing push order already closes).

## Q1 — Tag grammar

The tag is a **lookup key over the `.whl` file hash** (from PEP 751 `hashes.sha256`,
`crates/ocx_python/src/lock.rs:129-141`). It is *not* the stored layer's digest — the layer digest
is `sha256:<hash-of-repacked-tar>` (repack happens in `python_prepare.rs`). Three distinct hashes
coexist: the `.whl` hash (the tag), the repacked-tar digest (the layer descriptor), and the manifest
digest. The mount resolves by **tar digest**; the tag exists only for discovery/idempotency.

### Considered Options

**Option 1 — `sha256.<64-hex>` (period, full hex).** Algorithm-prefixed, dot separator, full digest.
**Option 2 — `sha256-<64-hex>` (dash, full hex).** Matches cosign/SOCI/Referrers Tag Schema.
**Option 3 — `<bare-64-hex>` (status quo) or truncated (`sha256.<32-hex>`).**

| Criterion (weight) | Opt 1 `sha256.<hex>` | Opt 2 `sha256-<hex>` | Opt 3 bare / truncated |
|--------------------|----------------------|----------------------|------------------------|
| R2 compliance (×3) | full — canonical `sha256.<hash>` | prefix ok, wrong separator | fails (no prefix / lossy) |
| No OCI-1.1 referrer-scan collision (×3) | high — period is not the referrer grammar | low — dash IS the referrer grammar | high (bare) / high (trunc) |
| Collision resistance (×2) | full 256-bit | full 256-bit | trunc = 128-bit, weaker |
| Readability / convention proximity (×1) | close to familiar dash | the convention | bare is opaque |
| OCI tag-charset legality (×2) | legal (71 chars) | legal (71 chars) | legal |
| Reversibility (×2) | one-way (published tags persist) | one-way | one-way |
| **Weighted total** | **strongest** | mid | weak |

### Decision — Option 1: `sha256.<64-hex>`, full hex, period separator.

**Rationale:** R2 mandates `sha256.<hash>` exactly. Full hex keeps the 256-bit collision floor (a
tag collision would require a SHA-256 preimage on the wheel bytes — treated as impossible; the lock
is the sole resolver, so there is no ambiguity path even in principle). The **period** is a
deliberate divergence from the dominant dash convention *because* the dash now has spec-defined
referrer semantics the research flagged (finding 1) — these tags are primary self-addressed CAS
keys, not referrers of any subject. 71 chars, well under the 128 cap; ocx's `Identifier` parser
accepts it verbatim. Truncation is rejected (needless collision-surface reduction; "lazy means
writing less code, not picking the flimsier algorithm").

**Collision/ambiguity stance:** the tag and the layer digest are independent hashes of independent
byte streams (`.whl` vs repacked tar). Idempotency (Q5) keys on the tag; mount correctness keys on
the tar digest. Repack determinism is the invariant that keeps them aligned (see Q5 edge case).

## Q2 — Minimal OCI manifest wrapping each stored layer

### Considered Options

**Option 1 — Empty-config artifact manifest.** Image manifest, `artifactType:
application/vnd.ocx-mirror.wheel.v1`, config `= application/vnd.oci.empty.v1+json` (`{}`,
digest `sha256:44136fa3…`), exactly one `tar+zstd` layer, **no** `subject`/referrers, **no**
platform field, annotations carry the original wheel filename.
**Option 2 — Reuse the current minimal `Bundle` package metadata config** (status quo:
`write_wheel_registration_metadata`).
**Option 3 — OCI 1.1 `subject`/Referrers graph-linking** to the consuming env package.

| Criterion (weight) | Opt 1 empty-config artifact | Opt 2 minimal Bundle | Opt 3 subject/referrers |
|--------------------|-----------------------------|----------------------|-------------------------|
| R1 fidelity — no package semantics (×3) | full — no `Metadata` at all | violates — a real `Bundle` | full but heavier |
| Registry compat vs `:5000`/GHCR/Harbor (×3) | high (empty-config sentinel, no referrers) | high | low — Referrers API uneven |
| Consistency with ocx prior art (×2) | matches description/patch artifacts | package path | none |
| D5 immunity (×3) | full — no AuthoringMetadata gate | fails post-bump (needs recorded platform) | fails |
| Implementation cost (×1) | needs a raw-artifact push (see Q6) | already exists | high |
| Reversibility (×2) | two-way for shape; one-way for published manifests | one-way | one-way |
| **Weighted total** | **strongest** | mid (fails D5) | weak |

### Decision — Option 1: empty-config artifact manifest, no `subject`, no platform field.

**Concrete shape:**

```jsonc
{
  "schemaVersion": 2,
  "mediaType": "application/vnd.oci.image.manifest.v1+json",
  "artifactType": "application/vnd.ocx-mirror.wheel.v1",
  "config": {
    "mediaType": "application/vnd.oci.empty.v1+json",
    "digest": "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
    "size": 2
  },
  "layers": [
    {
      "mediaType": "application/vnd.oci.image.layer.v1.tar+zstd",
      "digest": "sha256:<repacked-tar-digest>",
      "size": <n>,
      "annotations": { "org.opencontainers.image.title": "<original-wheel-filename>.whl" }
    }
  ]
}
```

**Rationale:** This is R1 in literal manifest form — no `Metadata`, no `Bundle`, no entrypoints/deps.
It is ocx's *third* instance of the empty-config + `artifactType` + no-`subject` pattern already used
by `push_description`/`push_patch_descriptor` (`external/ocx/.claude/artifacts/research_oci_config_artifact.md`
ruled subject/referrers out for exactly this reason). No `platform` field: R2/R4 keep platform
author-side, so a storage manifest is platform-free — this also *deletes* the current wart where
`push_wheel_layer` passes an arbitrary leg's `-p` key (§Consequences). Annotations carry only the
original wheel filename (`org.opencontainers.image.title`) for human/debug provenance; wheel ABI/build
tags are **not** annotated (R4 — no registry-side platform metadata, and the filename already encodes
them). `artifactType` `application/vnd.ocx-mirror.wheel.v1` is a registered custom type; the
empty-config sentinel is the OCI-registered value that dodges vendor `config.mediaType` allow-lists
(research finding 4).

## Q3 — Pipeline ordering, migration, backfill

### Considered Options

**Option 1 — Inline register-before-push (status quo ordering, reused).** Keep the storage publish
as a helper called per green `(V,P)` leg immediately before `invoke_env_push`, within the existing
serial Phase 2 loop.
**Option 2 — New dedicated pipeline phase** (`pipeline register-wheels`) between prepare and push,
publishing all storage entries first.
**Option 3 — Carry wheel `sha256` + repo in `plan.json`** (new `PlanAssetEntry` field) and publish
storages during a plan/prepare-time pass.

| Criterion (weight) | Opt 1 inline | Opt 2 new phase | Opt 3 plan-carried |
|--------------------|--------------|-----------------|--------------------|
| Mount-before-consume guarantee (×3) | full — same serial leg, storage first | full but adds a CI job | weak — decoupled timing |
| Fits Phase-2 serial invariant (×3) | native | new job + artifact plumbing | changes plan schema |
| Blast radius / new surface (×2) | none (helper already exists) | new subcommand + workflow template | plan schema v2→v3 |
| Backfill correctness (×2) | native — tag-exists check re-registers gaps | native | needs plan re-derivation |
| Concurrency win (×1) | serial (bounded by push loop) | could parallelize | plan-time parallel |
| Reversibility (×2) | two-way | one-way (workflow surface) | one-way (schema) |
| **Weighted total** | **strongest** | mid | weak |

### Decision — Option 1: keep the inline register-before-push ordering; formalize naming only.

**Rationale:** The status-quo ordering already satisfies "storage published and confirmed present
before the env push mounts it" and lives entirely inside the serial Phase-2 loop — no new phase, no
new CI job, no `plan.json` growth. The change is purely the **naming/tag grammar** (Q1/Q2) plus the
push mechanism (Q6). A dedicated phase (Opt 2) would touch the high-blast-radius generated-workflow
surface for zero correctness gain; carrying hashes in `plan.json` (Opt 3) forces a schema bump and
re-derivation for a value the push loop already recomputes. YAGNI on both.

**Migration of current app-repo wheel registration:** the feature is unreleased on `feat/pypi-mirror`
— **no back-compat obligation.** The migration is a rename of the naming convention and a swap of the
push mechanism:
- `wheel_reference` (`crates/ocx_python/src/naming.rs`): repository `<scope>/<index-host>/<package>`
  → **fixed** `pypi/<package>` (drop scope + index-host); tag `<bare-hex>` → `sha256.<hash>`.
- The `:from=<wheel_repository>` mount tail (`python_push.rs::build_env_push_args`) is **unaffected**
  by the tag change — it uses the repository path only (no tag), so it just tracks the new `pypi/...`
  path.
- `wheel_tag_exists` compares against the new canonical tag.

**Backfill semantics:** storage publication is **not** a tracked pipeline state — it is derived per
run from the fail-safe tag-exists check. A `BackfillPartial` version replays its green legs; each
leg's `register_wheel_layers` re-checks each wheel's `pypi/<name>:sha256.<hash>` tag and publishes
only the genuinely absent ones. No wheel-level backfill primitive is introduced (none exists;
`VersionPlatformMap` is `(version, platform)`-granular and stays that way). Content addressing makes
this safe: a re-registered wheel is byte-identical.

## Q4 — GC / retention policy for shared storage repos

### Considered Options

**Option 1 — Rely on registry-wide mark/sweep; no mirror-side GC.** Document the safety model +
race caveat; advise operators on Harbor/zot retention.
**Option 2 — Mirror-side retention command** (`pipeline gc-wheels`) that untags/deletes unreferenced
storage tags.

| Criterion (weight) | Opt 1 rely on registry GC | Opt 2 mirror-side GC |
|--------------------|---------------------------|----------------------|
| Correctness (blob survives while referenced) (×3) | full — global mark/sweep guarantees it | risks deleting a referenced blob |
| New surface / maintenance (×3) | none | new subcommand + delete-scope creds + tests |
| Matches codebase precedent (×2) | yes — no GC/cleanup task exists anywhere | none |
| Storage-cost control (×1) | passive (untagged storage manifests are tiny) | active |
| Reversibility (×2) | two-way | one-way (destructive) |
| **Weighted total** | **strongest** | weak |

### Decision — Option 1: rely on registry-wide mark/sweep; **no** mirror-side GC.

**Rationale:** In the reference `distribution` model (and Harbor/zot on the same substrate) a blob
survives GC as long as **any** manifest anywhere references its digest. Once an env package has
mounted a wheel blob, the `pypi/<name>:sha256.<hash>` storage manifest is no longer a keep-alive
anchor — its only job is to be a discoverable mount *source*. Storage manifests are tiny
(empty config + one descriptor); leaving them untagged/orphaned costs manifest bytes, not layer
bytes. The codebase has **zero** GC/cleanup precedent (no `oras`, no cleanup task) — inventing one
now is over-building (KISS/YAGNI).

**Interaction with Q5 (recorded).** Foregoing GC means `pypi/<name>` accumulates one tag per unique
wheel binary ever mirrored and never shrinks — the one cost lever this decision pulls on Q5, whose
precheck (`list_target_tags`, a full `tags/list` fetch) scales with that never-shrinking tag count.
Q5 records the bounded-cost `HEAD`-by-tag upgrade path (Option 4 there) for when a package's tag list
actually grows hot; per-package counts stay small in practice (one per distinct wheel binary, not per
app/version), so no action is taken now.

**The one real risk** is a retention policy that untags a storage manifest **before** the first
consumer mounts its blob (a push→mount race). The existing serial push order closes this: the
storage push completes and is tag-confirmed *before* the same leg's env push mounts. **Operator
guidance (docs, not code):** on Harbor/zot in production, exempt the `pypi/*` namespace from
aggressive "delete untagged" policies, or set `gcDelay`/grace ≥ the mirror's prepare→push latency.
This is a config recommendation, not a mirror responsibility.

## Q5 — Idempotency when the tag already exists

### Considered Options

**Option 1 — Tag-exists precheck, skip on hit (status quo, fail-safe).** `list_target_tags`
(issue #157 semantics); an authoritative hit skips the push; a transient error is swallowed (warn)
and the env push falls back to upload.
**Option 2 — Overwrite unconditionally.**
**Option 3 — Verify-then-skip:** fetch the manifest and compare the layer digest before deciding.
**Option 4 — Targeted manifest-exists (`HEAD` by tag).** Replace the `tags/list` fetch with a single
`HEAD /v2/<repo>/manifests/sha256.<hash>` — O(1) per wheel, cost independent of the repo's historical
tag count (which, per Q4, never shrinks). Trade-off: needs a fresh #157 fail-safe classification
(not-found vs. transient) on the `HEAD` path — the existing distinction lives only in
`list_target_tags` today.

| Criterion (weight) | Opt 1 precheck-skip | Opt 2 overwrite | Opt 3 verify-then-skip |
|--------------------|---------------------|-----------------|------------------------|
| Correctness given content-addressing (×3) | full — matching tag ⇒ matching content by construction | pointless re-push | full but redundant |
| Fail-safe #157 compliance (×3) | full | ignores | full |
| Registry round-trips per wheel (×2) | 1 (tags/list) | 1 (push) | 2 (list + manifest fetch) |
| Matches existing `wheel_tag_exists` (×2) | identical | rewrite | rewrite |
| Reversibility (×1) | two-way | two-way | two-way |
| **Weighted total** | **strongest** | weak | mid |

### Decision — Option 1: tag-exists precheck, skip on authoritative hit; fail-safe on error.

**Rationale:** The tag **is** the wheel's content hash. A present `sha256.<hash>` tag can only have
been produced by the same wheel bytes (SHA-256), so a hit is definitionally the same content — no
manifest fetch is needed to confirm it (Opt 3's second round-trip buys nothing). This is exactly the
existing `wheel_tag_exists` behavior; keep it. Preserve the #157 fail-safe: `list_target_tags`
returns an authoritative not-found → publish; a transient error → **warn and skip registration** (the
env push then falls back to a full upload — load-bearing, not a regression). No overwrite: re-pushing
identical content is wasted work.

**Cost interaction with Q4 (recorded, not blocking).** Option 1 reuses `wheel_tag_exists` →
`list_target_tags`, a full `tags/list` fetch whose cost is proportional to the repo's tag count —
which Q4's no-GC decision guarantees only ever grows. The two decisions compound. This is acceptable
now: the #157 not-found/transient classification lives in `list_target_tags` and is reused verbatim
(zero new fail-safe surface), and per-package tag counts stay small (one per distinct wheel binary,
not per app/version). **Bounded-cost upgrade path (Option 4):** if a hot package's `pypi/<name>` tag
list grows large enough to matter, swap the precheck for a targeted
`HEAD /v2/<repo>/manifests/sha256.<hash>` (O(1), tag-count-independent). Deferred until a real
tag-count hotspot appears (YAGNI), and gated on porting the #157 not-found-vs-transient distinction to
the `HEAD` path so the fail-safe contract is preserved.

**Edge case (invariant dependency, not re-litigated):** idempotency keys on the `.whl` hash (tag)
while mount correctness keys on the **repacked-tar** digest. If repack were non-deterministic, the
tag could exist (skip push) while the mount misses (different tar digest) → silent fallback to
upload. Repack determinism is an existing `ocx_python::repack_wheel` property; this design **depends**
on it and the design spec records it as a test obligation.

## Q6 — Push mechanics under D5

This is the crux. On the current `feat/layer-mount` submodule, `register_wheel_layers` shells
`ocx package push -p <platform> -i <registry>/pypi/<name>:sha256.<hash> -m <bundle> <layer>`. After
the mandated bump to main (R5), that path **breaks**: `ocx package push` requires an
`AuthoringMetadata` sidecar with a recorded platform (from `ocx package create --platform`), enforces
`--platform == recorded` (D5), and runs `verify_dependency_pins` — none of which a non-package
artifact can or should satisfy. R1 says storages are not packages, so they must not travel the
package-push path at all.

### Considered Options

**Option 1 — New additive `ocx_lib` public artifact-push API, called directly (no subprocess).**
Expose a thin `push_artifact(identifier, artifact_type, layer, annotations)` on
`ocx_lib::oci::Client` that reuses the **`push_description` mechanism** (`client.rs:1101-1179`): a
`push_blob` loop for the empty-config `{}` blob and the layer, then a `ManifestBuilder` carrying
`artifact_type` + the empty-config sentinel, then `push_manifest_raw` to the tag. That path never
constructs an `Info` or `Metadata` — unlike the mount-driven `push_multi_layer_manifest`
(`client.rs:872`, `push_multi_layer_manifest(&self, package_info: &Info, layers: &[LayerRef])`),
which takes a full `&Info` and drives mount-then-upload logic irrelevant to a first-time content
push. `push_description`/`push_patch_descriptor` are the exact precedent — same empty-config +
custom-`artifactType` machinery, no `Info` at all. Land it in the **same submodule bump** that
rebases onto main. ocx-mirror already builds a `Publisher` (authed `Client`) for the fail-safe tag
reads in the same push loop → call `Publisher`'s one-line forward directly, no subprocess, no
metadata sidecar file.
**Option 2 — Raw `ocx_lib::oci::native::Client` escape hatch.** Drive the re-exported patched fork
client (`push_blob` + `push_manifest_raw`) directly from ocx-mirror. No upstream change, full
media-type/tag freedom — but ocx-mirror must reimplement auth/token caching that `Client`/`Publisher`
already own.
**Option 3 — Keep shelling `ocx package push` with a compatibility shim.** Add an `ocx package
create --platform` step + empty dependency set to satisfy D5.

| Criterion (weight) | Opt 1 new `ocx_lib` API (direct) | Opt 2 raw native client | Opt 3 shim `ocx package push` |
|--------------------|----------------------------------|-------------------------|-------------------------------|
| R1 fidelity (×3) | full — empty-config artifact, no `Metadata` | full | violates — fakes a package |
| D5 immunity (×3) | full — bypasses AuthoringMetadata/pin gate | full | fights D5 forever |
| Reuses existing auth (×3) | yes — the Publisher already authed here | **no** — reimplement token cache | yes (ocx binary) |
| Upstream-coordination cost (×2) | one additive method in the bump commit | none | none but permanent tax |
| Reuses ManifestBuilder + empty-config pattern (×2) | yes (3rd instance) | manual manifest assembly | no |
| Removes temp-file metadata dance (×1) | yes | yes | no |
| Reversibility (×2) | two-way (additive API) | two-way | one-way (shim rot) |
| **Weighted total** | **strongest** | mid (auth cost) | weak |

### Decision — Option 1: add an additive `ocx_lib` artifact-push API and call it directly from `register_wheel_layers`; **no subprocess, no metadata sidecar.**

**Rationale:** The rebase onto main is already a required, in-flight change with known compile fixes
(§Upstream Alignment). Bundling one small, genuinely-reusable public method into that same commit is
the boring, coordinated path — and it is the *same* empty-config + `artifactType` machinery ocx
already uses internally twice (the `push_description` path — `push_blob` loop + `ManifestBuilder` +
`push_manifest_raw` — **not** the `Info`-driven `push_multi_layer_manifest`), so it is
upstream-general, not a mirror hack. ocx-mirror already constructs a `Publisher` (an authed `Client`)
for the fail-safe tag reads **in the same push loop**; calling `Publisher`'s one-line forward to that
same client is strictly simpler than the current subprocess +
byte-identical-temp-file dance (`write_wheel_registration_metadata` is deleted). It is R1-pure
(no `Metadata` ever constructed) and D5-immune (never touches `AuthoringMetadata`, `resolve_platform`,
or `verify_dependency_pins`).

**API contract (upstream, to land in the bump):**

```rust
// Implemented on `ocx_lib::oci::Client`; `Publisher` carries a one-line forward
// (`self.client.push_artifact(..)`), exactly as `Publisher::push_description`
// forwards (publisher.rs:118). ocx-mirror holds a `Publisher`, so it calls the forward.
/// Push a single-layer, empty-config artifact under `artifact_type` to `identifier`,
/// bypassing package `Metadata` entirely. Reuses `ManifestBuilder` + the empty-config
/// sentinel — the same path as `push_description`/`push_patch_descriptor`.
pub async fn push_artifact(
    &self,
    identifier: &oci::Identifier,        // registry + pypi/<name> + tag sha256.<hash>
    artifact_type: &str,                 // "application/vnd.ocx-mirror.wheel.v1"
    layer: LayerRef,                     // File(<repacked-tar path>) — media type inferred
    annotations: BTreeMap<String, String>,
) -> Result<oci::Digest>;
```

The parameter is `&oci::Identifier`, **not** `native::Reference`: the transport type is deliberately
non-public (`client.rs:154` — the `From<&Identifier> for native::Reference` impl is removed, no public
bypass), and every cited precedent (`push_description` at `client.rs:1101`/`publisher.rs:118`,
`push_patch_descriptor` at `client.rs:1203`) takes `&Identifier` and builds the canonical reference
internally. ocx-mirror already constructs exactly this type for the same lookup — `wheel_tag_exists`
(`python_push.rs:248`) builds `Identifier::new_registry(wheel_repository, registry)`.

**Escape hatch (documented, not chosen):** if landing an upstream method is undesirable, Option 2
(raw `native::Client`) is the only path needing no `ocx_lib` change — at the cost of reimplementing
auth in ocx-mirror. Recorded here so the trade-off is not silently forgotten.

## Upstream Alignment

### Unified-platform-model (main `9978cc2`) implications

R5 is the alignment target. Two concrete `ocx-mirror` compile breaks on the submodule bump (both
tiny, mechanical — `Platform::Specific` lost `os_version` + `features`):

| Site | Current | Fix on bump |
|------|---------|-------------|
| `crates/ocx_mirror/src/spec/wheels.rs:69-84` | destructures `os_version` and rejects `variant.is_some() \|\| os_version.is_some()` | drop `os_version` from the destructure; the "no variant/os_version in wheels keys" validation collapses to the `variant` check only (os_version no longer exists to reject). Keep `os_features` (libc axis, unchanged per R5) and `variant`. |
| `crates/ocx_python/src/compose.rs:526-533` | `Platform::Specific { …, os_version: None, os_features: Vec::new(), features: None }` | remove the `os_version: None` and `features: None` fields; keep `os_features: Vec::new()` |

Positive alignment consequence: the Q2 decision (**no platform field on storage manifests**) removes
the `push_wheel_layer` `-p <arbitrary-leg-key>` wart entirely — storages become genuinely
platform-free, which is exactly what R2/R4 want and what the unified model simplifies toward.
Storage push is unaffected by D5's recorded-platform binding because (Q6) it never travels the
package-push path.

### `feat/layer-mount` rebase-onto-main conflict strategy

**`crates/ocx_lib/src/publisher.rs` — the one substantive conflict.** main carries `Publisher::push`/
`push_cascade` taking **`Vec<Info>`** (one `Info` per platform, sequential fan-out for multi-tag
pinning + `publish_gate`); `feat/layer-mount` carries the older single-`Info` signature plus
`PushOutcome { layer_counts: oci::LayerCounts }` (the 3-tuple mount/upload/verify counters).

**Recommended reconcile: take main's `Vec<Info>` fan-out as the base, graft layer-mount's
`LayerCounts` onto it.**
- Keep main's `push(Vec<Info>, …)` signature and its per-platform sequential loop + image-index merge
  + `verify_dependency_pins` gate (D5).
- Union the outcome: `PushOutcome { manifest_digest, cascade_tags, layer_counts: oci::LayerCounts }`.
  Accumulate `LayerCounts` **across** the `Vec<Info>` iterations (sum `mounted`/`uploaded`/`verified`
  over each platform's `Client::push_package` return — main's `push_package` must adopt layer-mount's
  3-tuple return `(Digest, Manifest, LayerCounts)`).
- The mount call site (`try_mount_layer`, `push_multi_layer_manifest`'s `LayerRef` arms) is
  **orthogonal** to the fan-out — it lives below `push_package` and survives the rebase unchanged.
- `test_transport.rs` stub conflict is trivial: keep both — main's stub shape plus layer-mount's
  `mount_results` FIFO queue on `StubTransport` (they do not overlap semantically).

**Downstream ocx-mirror effect:** `LayerReuse` (`run_summary.rs:85-89`) already mirrors `LayerCounts`
verbatim and parses `EnvPushReport.layers` with `#[serde(default)]` — so the reconciled `PushOutcome`
keeps ocx-mirror's run-summary layer stats working with **zero** mirror-side change. The new
`push_artifact` (Q6) is additive and does not participate in the `Vec<Info>` fan-out (storages are
single, flat, un-cascaded).

## Decision Outcome (summary)

| Q | Decision |
|---|----------|
| Q1 | `sha256.<64-hex>`, full hex, **period** separator (deliberate divergence from the dash referrer grammar) |
| Q2 | Empty-config artifact manifest: `artifactType: application/vnd.ocx-mirror.wheel.v1`, `application/vnd.oci.empty.v1+json` config, one `tar+zstd` layer, **no** `subject`, **no** platform field, wheel-filename annotation only |
| Q3 | Keep inline register-before-push in the serial Phase-2 loop; rename to `pypi/<name>:sha256.<hash>`; no new phase, no plan-schema growth; backfill is derived from the fail-safe tag-exists check |
| Q4 | Rely on registry-wide mark/sweep; **no** mirror-side GC; operator guidance for Harbor/zot retention |
| Q5 | Tag-exists precheck, skip on authoritative hit, fail-safe on transient error (status quo `wheel_tag_exists`) |
| Q6 | Additive `Client::push_artifact(&oci::Identifier, …)` (empty-config artifact via the `push_description` mechanism, **not** `push_multi_layer_manifest`), with a one-line `Publisher` forward; called **directly** on the already-authed client — no subprocess, no metadata sidecar; D5-immune, R1-pure |

### Consequences

**Positive:**
- R1-literal manifests (no `Metadata`), D5-immune push, zero client-side change (R3 holds — pull/GC
  are provenance-blind to mounted layers).
- Deletes the `-p <arbitrary-key>` platform wart and the `write_wheel_registration_metadata`
  byte-identical temp-file dance.
- No new pipeline phase, no `plan.json` schema bump, no GC subcommand — minimal surface.
- Run-summary layer stats survive the rebase unchanged.

**Negative:**
- Q6 requires an additive upstream `ocx_lib` method landed in the bump commit (coordination cost;
  mitigated by it being genuinely reusable and small).
- `artifactType`/empty-config is spec-legal but historically uneven across registries → acceptance
  coverage against `:5000` is mandatory (not optional).

**Risks:**
- *Push→mount race under aggressive retention* → mitigated by the existing serial push order +
  operator guidance (Q4).
- *Non-deterministic repack* would silently degrade idempotency to upload → recorded as a test
  obligation (Q5 edge case).
- *A registry that rejects the empty-config sentinel* → acceptance test surfaces it; fallback is the
  documented raw-native escape hatch, but the `:5000` distribution fixture accepts it.

## Validation

- [ ] Acceptance test: env push mounts a `pypi/<name>:sha256.<hash>` storage layer against `:5000`
      (mount-hit path) and reports `mounted > 0`.
- [ ] Acceptance test: second run is idempotent (tag-exists skip, no re-push).
- [ ] Unit test: `wheel_reference` renders `pypi/<package>:sha256.<hash>` (drops index-host/scope).
- [ ] Unit test: repack determinism (same wheel → same tar digest) — the Q5 invariant.
- [ ] Security: mount stays same-registry (structural, R3); storage read-scope is a single stable grant.
- [ ] `task verify` passes on implementation.

## Links

- [`design_spec_pypi_layer_storage.md`](./design_spec_pypi_layer_storage.md)
- [`research_pypi_layer_storage.md`](./research_pypi_layer_storage.md)
- [`adr_pypi_lock_derivation.md`](./adr_pypi_lock_derivation.md)
- [`adr_ocx_python_crate.md`](./adr_ocx_python_crate.md)
- `external/ocx/.claude/artifacts/research_oci_config_artifact.md` (ocx's empty-config decision)

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-07-18 | Architect | Initial draft — Q1–Q6 decided within R1–R5 |
| 2026-07-18 | Architect | Adversarial-review fixes: Q6 `push_artifact` takes `&oci::Identifier` (not `native::Reference`); mechanism corrected to the `push_description` path (not `push_multi_layer_manifest`); ownership resolved to `Client` + one-line `Publisher` forward; Q4×Q5 cost interaction + `HEAD`-by-tag bounded-cost upgrade path recorded |
