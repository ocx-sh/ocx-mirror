# Research: PyPI Wheel Layer-Storage Namespace in OCI Registries

<!--
Condensed digest for adr_pypi_layer_storage.md / design_spec_pypi_layer_storage.md.
Full source dump lives in the parent task record; this is the cited condensation.
-->

## Metadata

**Date:** 2026-07-18
**Domain:** packaging
**Triggered by:** Formalizing ocx-mirror's `pip-packages/<host>/<name>:<bare-hash>` wheel
registration into `<registry>/pypi/<name>:sha256.<hash>` content-addressed storage repos.
**Expires:** 2027-01 (re-verify registry Referrers/GC behavior — fast-moving)

## Direct Answer

Nobody has shipped a widely-adopted **wheel-*layer*-granular, content-addressed, cross-package
dedup** OCI namespace. Every adjacent project (PyOCI, ocipy, npm-registry-oci, conda-oci-mirror)
treats OCI as a blob-store substitute at **package-version** granularity, not sub-package/wheel
granularity. ocx-mirror's design is **novel for this ecosystem** — borrow *conventions* (tag
grammar, empty-config artifact shape, GC-safety reasoning) from battle-tested adjacent patterns,
not a whole design from any one project.

## Key Findings

1. **Tag grammar: the ecosystem uses `<algo>-<hex>` (dash), now normative** as the OCI Referrers
   Tag Schema fallback (`sha256-<hex>`, algo≤32 chars, encoded≤64 chars). cosign (`sha256-….sig`),
   SOCI (`sha-<digest>`), and the image-spec itself all converged on the dash. *But that grammar now
   carries spec-defined "I am a referrer of subject digest X" semantics.* ocx-mirror wheel tags are
   **primary self-addressing CAS keys**, not referrers — reusing the dash risks OCI-1.1-aware tooling
   misreading them as orphaned referrer stubs during referrer-emulation `tags/list` scans. →
   **`sha256.<hex>` (period) is the deliberate, legal divergence.** [image-spec manifest.md;
   Sigstore cosign docs; Chainguard "OCI v1.1 in cosign"; SOCI index-manifest-v2]
2. **Tag charset is `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`** (max 128, first char alnum/underscore).
   Both `sha256.<64hex>` (71 chars) and `sha256-<64hex>` are legal — convention choice, not a
   grammar constraint. ocx's own `Identifier` parser defers charset validation to the
   fork/registry; `sha256.<64hex>` parses cleanly. [distribution-spec]
3. **Manifest shape: image manifest + `artifactType` + explicit empty config
   (`application/vnd.oci.empty.v1+json`, `{}`), one real layer, NO `subject`/referrers.** This is
   already ocx's own established pattern (description + patch artifacts,
   `push_description`/`push_patch_descriptor`). ocx's prior research
   (`external/ocx/.claude/artifacts/research_oci_config_artifact.md`) explicitly ruled: *"Don't use
   subject/Referrers API; ocx resolves by explicit tag already."* Wheel storage = the third instance
   of this pattern, not a new one. [image-spec; ocx prior art]
4. **Empty-config + custom artifactType is spec-legal but was NOT universally implemented** —
   `containers/image` (skopeo/podman/buildah) rejected `application/vnd.oci.empty.v1+json` until a
   fix; zot had the same class of bug (#2977); Quay/Artifactory historically gated custom
   `config.mediaType` behind allow-lists. The registered empty-config sentinel dodges vendor
   allow-lists that an arbitrary custom config type would trip. Conformance-test per target
   registry, don't assume. [containers/image#2279; zot#2977]
5. **Cross-repo blob mount** `POST /v2/<target>/blobs/uploads/?mount=<digest>&from=<source>` →
   `201` on hit, silently degrades to a normal `202` chunked upload on miss (not a hard error —
   never assume the mount happened). Requires **read access to the source repo** on top of target
   write — a stable `pypi/*` read-scope grant is cleaner than every env repo needing read on every
   other env repo. [distribution HTTP API]
6. **GC safety (registry-wide mark/sweep):** reference `distribution` stores blobs in ONE global
   content-addressed store; each repo holds thin link pointers. GC marks by scanning *every* manifest
   in *every* repo → a blob survives as long as **any** manifest anywhere references its digest.
   Consequence: once an env package has mounted the wheel blob, deleting/untagging the
   `pypi/<name>:sha256.<hash>` storage manifest does **not** orphan the blob. The storage tag's only
   job is to be a discoverable mount *source*, not a keep-alive anchor. **Real risk = a retention
   policy untagging the storage manifest BEFORE the first consumer mounts** (a push→mount race), not
   after. Harbor/zot layer per-repo retention on top of the same global substrate — verify empirically.
   [distribution GC docs; Harbor GC docs; zot]
7. **GHCR sidesteps mount entirely** via shared-bucket storage (identical blob in any repo is a cheap
   existence check) — but this is registry-specific and does NOT generalize to
   distribution/Harbor/zot. It also produced a real repo-name-leak security bug in exactly this area.
   [Chainguard "GHCR private repos sometimes weren't"]
8. **`artifactType` itself is battle-tested** (image-spec v1.1.0, Feb 2024). The uneven feature is
   `subject`/Referrers API graph-linking — which this design does not use.

## Adjacent Prior Art (all experimental / not directly transferable)

| Project | Shape | Gap vs this design |
|---------|-------|--------------------|
| PyOCI (Rust) | `<registry>/<ns>/<package>`, wheel builds as platform entries under one image; refuses re-upload if `(name,version,arch)` exists | Package-version granularity; no wheel-layer CAS, no env composition |
| ocipy | ORAS wrapper, one tarball one tag | No content-addressed layer scheme |
| npm-registry-oci / Aarti | OCI as blob store under npm protocol | No cross-package dedup beyond registry digest storage |
| conda-oci-mirror | 1:1 version-tag mirror into ghcr.io | Relies on native blob-digest addressing; no hash tags |
| Homebrew bottles | version tag; per-blob hash-tagging PR **rejected** ("spams fake tags, makes tag list useless") | Validates avoiding hash tags in a *listable* namespace — but they rely on GHCR shared-bucket dedup |
| apko/melange (Wolfi) | N APK layers → composed OCI image | Validates "many small package layers → composed image" shape; APKs distributed via flat index, not per-package hash-tagged repos |
| GitLab PyPI/npm virtual registries | pull-through cache | Roadmapped 2026, not shipped |

## Recommendations (feeding the ADR)

1. **Prefix the tag with the algorithm** — the current bare-hex tag is the one clearly sub-par choice
   vs all prior art.
2. **`sha256.<hex>` (period), full hex** — deliberate divergence from the dash Referrers grammar; see
   finding 1.
3. **Model the manifest on ocx's own empty-config artifact pattern** (finding 3) — fewest
   registry-compat landmines, consistent with the codebase's own prior decision.
4. **Namespace `pypi/<name>`, drop the source-host segment** — host is a fetch-time detail, not
   package identity; PyPI is the one canonical index this models.
5. **Rely on registry-wide mark/sweep for GC safety, but push storage-repo manifest and confirm it
   present BEFORE the env mount** (already the pipeline order) to close the push→mount race; if
   targeting Harbor/zot, exempt `pypi/*` from aggressive untag policies or set gcDelay ≥ pipeline
   latency. No mirror-side GC command (YAGNI).

## Sources

| Source | Type | Relevance |
|--------|------|-----------|
| OCI image-spec manifest.md | Spec | artifactType, empty config, Referrers Tag Schema |
| OCI distribution-spec spec.md | Spec | tag charset `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}` |
| distribution.github.io HTTP API v2 | Docs | cross-repo mount `?mount=&from=` |
| distribution.github.io Garbage Collection | Docs | global mark/sweep model |
| Sigstore cosign; Chainguard "OCI v1.1 in cosign" | Docs/Blog | dash-grammar referrer semantics |
| awslabs/soci-snapshotter index-manifest-v2 | Repo | `sha-<digest>` tag prior art |
| containers/image#2279; project-zot/zot#2977 | Issues | empty-config non-universality |
| goharbor Harbor GC; zotregistry what's-new | Docs | per-repo retention on global substrate |
| Chainguard "GHCR private repos sometimes weren't" | Blog | shared-bucket dedup + security edge |
| AllexVeldman/pyoci; Homebrew/brew#19197 | Repo/PR | closest package-version prior art; rejected hash-tag PR |
| chainguard-dev/apko + melange | Repo | many-layers→composed-image validation |
| ocx `research_oci_config_artifact.md`, `oci_manifest_usage.md` | Local | ocx's own empty-config decision |
