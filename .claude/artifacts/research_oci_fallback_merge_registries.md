# Research: OCI referrers fallback merge and registry support (2026)

<!-- hex-plan research axis: OCI spec evolution / registry ecosystems. Plan: mirror-signing -->

## Metadata

**Date:** 2026-09-02
**Domain:** packaging
**Triggered by:** plan `mirror-signing` — how the mirror's fallback-index merge is made safe, which registry image the harness uses for the native-API leg, how destination capability is probed.
**Expires:** 2027-03-01 (distribution v3 referrers PR and zot conformance fixes are in flight)

## Direct Answer

1. **Conditional manifest PUT.** distribution-spec: "Clients MAY use a conditional HTTP push for registries that support ETag conditions … Protection against race conditions is the responsibility of clients and end users, and can be resolved by using a registry that provides the referrers API" ([spec.md](https://github.com/opencontainers/distribution-spec/blob/main/spec.md)). No registry-side atomicity is normative. go-containerregistry's `commitSubjectReferrers` (GET → append → PUT, no `If-Match`) has an open lost-update bug closed "not planned" ([go-containerregistry#2205](https://github.com/google/go-containerregistry/issues/2205), 2026-02). No tool ships the ETag retry loop. ocx's read-append-write-readback with `MAX_FALLBACK_ATTEMPTS=5` is ahead of the ecosystem norm.
2. **registry:3.** Does **not** implement the Referrers API as of v3.1.1 (2026-05-01); native support is an unmerged PR for a later milestone ([distribution#4828](https://github.com/distribution/distribution/pull/4828), 2026-03, "the one after" 3.2). Otherwise a drop-in for `registry:2` in compose. Use `registry:2`/`:3` only for the fallback-tag leg.
3. **zot.** v2.1.19/v2.1.20 (~2026-08) both shipped referrers-conformance fixes ([v2.1.19](https://github.com/project-zot/zot/releases/tag/v2.1.19)); `SyncLegacyCosignTags` (default `true`) is a documented sync key ([sync config](https://pkg.go.dev/zotregistry.dev/zot/v2/pkg/extensions/config/sync)). zot serves the native API, no fallback synthesis. Pin the harness to a digest, not `latest`.
4. **Capability probing.** Spec: 404 on the referrers endpoint ⇒ MUST fall back to the tag schema. In practice ECR returns **405** on `subject`-bearing pushes ([containers-roadmap#2783](https://github.com/aws/containers-roadmap/issues/2783), open 2026-03). Treat 404 and 405 as unsupported; a successful capability GET is not proof a subject-bearing PUT succeeds — observe per push attempt. No tool caches this robustly; oras-go's compat flag is user-set.
5. **Sidecar tags on copy.** cosign's scheme `sha256-<digest>.sig/.att/.sbom` ([SBOM_SPEC.md](https://github.com/sigstore/cosign/blob/main/specs/SBOM_SPEC.md)); `.sbom` attachments deprecated in favour of attestations, `.sig` lives on. cosign's own merge behaviour for multiple signatures on one `.sig` tag (layer append vs overwrite) **not authoritatively documented — open**. Harbor replication drops referrers while direct `oras copy --recursive` works ([harbor#23210](https://github.com/goharbor/harbor/issues/23210)) — the bulk-sync-path class of defect the mirror must not repeat.
6. **Depth in practice.** No tool-imposed depth cap and no published counts; cosign's own recursive `tree` walk is an open feature request ([cosign#4204](https://github.com/sigstore/cosign/issues/4204), [#4335](https://github.com/sigstore/cosign/issues/4335)). Referrers-of-referrers are spec-possible, untested by mainstream tools.

## Technology Landscape

- **Established:** zot as the reference native-Referrers registry for conformance; oras-go fallback-on-4xx.
- **Emerging:** distribution v3 native referrers (unmerged, no ETA).
- **Declining:** `.sbom` attachment tags.
- **Structural gap:** bulk sync/replication paths (Harbor, ECR recursive copy) lag direct-push referrer support.

## Key Findings

1. The ETag/`If-Match` loop is spec-suggested and unshipped ecosystem-wide; bounded read-back retry is the practical ceiling. [spec.md](https://github.com/opencontainers/distribution-spec/blob/main/spec.md), [#2205](https://github.com/google/go-containerregistry/issues/2205)
2. `registry:3` gains nothing for referrers this year. [distribution#4828](https://github.com/distribution/distribution/pull/4828)
3. 405 is a live "unsupported" signal alongside 404. [#2783](https://github.com/aws/containers-roadmap/issues/2783)
4. Harbor's silent drop is a copy-path defect with a working direct-copy workaround. [harbor#23210](https://github.com/goharbor/harbor/issues/23210)

## Recommendation

1. **Merge strategy:** keep ocx's read-append-write-readback with `MAX_FALLBACK_ATTEMPTS=5`; add `If-Match` opportunistically when the destination returns an `ETag`, never block on it.
2. **Harness:** zot at a pinned digest for the native leg; `registry:2` for the fallback leg.
3. **Probing:** 404 and 405 ⇒ fallback; treat every subject-bearing PUT's status as the truth, not a preflight GET.
4. **Acceptance:** one end-to-end test asserting a tagless, subject-linked referrer is copied and discoverable on both legs — the Harbor/ECR failure class.

## Sources

| Source | Type | Date | Relevance |
|--------|------|------|-----------|
| https://github.com/opencontainers/distribution-spec/blob/main/spec.md | Spec | current | conditional push MAY; 404 fallback MUST |
| https://github.com/google/go-containerregistry/issues/2205 | Issue | 2026-02, not planned | lost update in fallback merge |
| https://github.com/distribution/distribution/pull/4828 | PR | 2026-03, open | registry:3 referrers, future milestone |
| https://github.com/distribution/distribution/releases | Releases | 2026-05 | v3.1.1 |
| https://github.com/project-zot/zot/releases/tag/v2.1.19 | Release | 2026-08 | referrers conformance fixes |
| https://pkg.go.dev/zotregistry.dev/zot/v2/pkg/extensions/config/sync | Docs | current | `SyncLegacyCosignTags` |
| https://github.com/aws/containers-roadmap/issues/2783 | Issue | 2026-03, open | ECR 405 |
| https://github.com/goharbor/harbor/issues/23210 | Issue | open | replication drops referrers |
| https://github.com/sigstore/cosign/blob/main/specs/SBOM_SPEC.md | Spec | current | sidecar tag scheme |
| https://github.com/sigstore/cosign/issues/4204 · /4335 | Issues | open | recursive referrer walk unsupported |
