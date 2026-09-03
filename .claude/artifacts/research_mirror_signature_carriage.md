# Research: registry mirroring and signature carriage (prior art)

<!-- hex-discuss research lane: prior-art web scan. Discussion: .agents/discussions/mirror-signing.md -->

## Metadata

**Date:** 2026-09-01
**Domain:** packaging | security
**Triggered by:** discussion `mirror-signing` — whether/how ocx-mirror carries upstream signatures through registry copies, handles destinations without the OCI 1.1 Referrers API, signs its own pushes, and backfills.
**Expires:** 2027-03-01 (registry Referrers-API support and cosign defaults move quarterly)

## Direct Answer

**1. Referrer-graph copy.** [oras `cp --recursive`](https://oras.land/docs/commands/oras_cp/) walks referrers transitively and copies signatures/attestations/SBOMs — flagged `[Preview]`, no documented depth/cycle handling found. regclient's `regsync`/`regctl` support `referrers`/`referrerFilters`/`referrerSource`/`referrerTarget`, but referrers are **not copied by default** — opt-in only ([regclient.org/usage/regsync](https://regclient.org/usage/regsync/)). `cosign copy --only=sig,att,sbom` copies just the signature-shaped artifacts ([cosign_copy.md](https://github.com/sigstore/cosign/blob/main/doc/cosign_copy.md)). Harbor's replication service does **not** traverse the Referrers API at all — confirmed isolated to replication since direct-push and `oras copy --recursive` between the same two registries work ([harbor#23210](https://github.com/goharbor/harbor/issues/23210)). Zot's sync extension natively syncs referrers and added `SyncLegacyCosignTags` to selectively skip legacy `.sig`/`.att`/`.sbom` sidecar tags once referrers are native — sidecar tags are copied as ordinary tags unless filtered ([zot releases](https://github.com/project-zot/zot/releases)).

**2. Destination without the Referrers API.** distribution-spec: a 404 from `/v2/<name>/referrers/<digest>` MUST fall back to `GET /v2/<name>/manifests/sha256-<hex>` (colon→hyphen), expecting an equivalent image index ([spec.md](https://github.com/opencontainers/distribution-spec/blob/main/spec.md)). The spec's own merge procedure is read-then-check-then-append-then-push — **not atomic** — and names the failure mode: "multiple clients could attempt to update the tag simultaneously resulting in race conditions and data loss," resolvable only by a real Referrers-API registry or an optional ETag-conditional push. cosign hit this seam: pushing via the Referrers API to an older registry can fail to also push a valid fallback tag ([cosign#4641](https://github.com/sigstore/cosign/issues/4641)). ECR outright rejects referrer manifests carrying a `subject` field with 405/"Invalid JSON syntax," breaking `oras copy -r`; opened March 2026, still open ([containers-roadmap#2783](https://github.com/aws/containers-roadmap/issues/2783)).

**3. Re-sign vs preserve; trust expression.** No surveyed mirror (oras/regsync/skopeo/Harbor/zot) adds its own signature by default; skopeo's `--sign-by` is an explicit opt-in that adds an *additional* simple-signing signature at copy time, never a replacement ([skopeo-copy man](https://github.com/containers/skopeo/blob/main/docs/skopeo-copy.1.md)). Sigstore keyless trust binds to signer identity (`certificate-identity`/`certificate-oidc-issuer`), so a mirror re-signing under its own identity requires consumers to *add* a trust authority, not swap the upstream one ([policy-controller overview](https://docs.sigstore.dev/policy-controller/overview/)). Multi-signature AND/OR is **policy-engine-specific**: Kyverno's `attestors.count` defaults to all configured attestors (AND), tunable to a threshold ([Kyverno verify-images](https://kyverno.io/docs/policy-types/cluster-policy/verify-images/overview/)); a separate Kyverno-adjacent source describes any-one-attestor semantics — conflicting, unresolved. Notation's spec leaves multi-signature trust-policy representation an **open design question** ([trust-store-trust-policy.md](https://github.com/notaryproject/specifications/blob/main/specs/trust-store-trust-policy.md)).

**4. Backfill/sweep tooling.** No maintained first-party "sign everything unsigned in repo X" tool in cosign or oras — users script loops over `oras attach`/`cosign sign`. cosign 2.x signing is **not idempotent**: re-signing an already-signed image can fail with a Rekor `CreateLogEntryConflict`/UUID-mismatch error rather than a clean no-op ([cosign#3356](https://github.com/sigstore/cosign/issues/3356); fix PR referenced, merge/release not confirmed). No "already signed by identity X" primitive exists outside verify time.

**5. Incidents.** Harbor [#23210](https://github.com/goharbor/harbor/issues/23210) (2026, open) — replication silently drops OCI 1.1 referrers; only legacy `.sig`-tag artifacts and the base image survive. [#17107](https://github.com/goharbor/harbor/issues/17107) — earlier report of the same shape. [#21636](https://github.com/goharbor/harbor/issues/21636) — replication can **fail outright** when images carry signatures. [#22592](https://github.com/goharbor/harbor/issues/22592) — synced OCI-1.1 cosign signature classified "UNKNOWN". Harbor FAQ: pre-2.9.2 replication could land a signature before its subject. ECR [containers-roadmap#2783](https://github.com/aws/containers-roadmap/issues/2783) — hard-rejects referrer pushes, live as of 2026-03.

**6. Trajectory 2026.** OCI Distribution 1.1 shipped 2024-03 ([OCI blog](https://opencontainers.org/posts/blog/2024-03-13-image-and-distribution-1-1/)). Native support confirmed: Azure ACR, Zot. GHCR: no Referrers API ([GH community #163029](https://github.com/orgs/community/discussions/163029)). ECR: announced, 405s in practice. GitLab: accepts `subject`, no full API. Harbor: referrers on direct writes, not through replication. GAR: unverified. Cosign v3 defaults to the Sigstore bundle as referrer, falls back to the referrers tag, keeps sidecar tags via `--new-bundle-format=false` ([cosign v3 blog](https://blog.sigstore.dev/cosign-3-0-available/), [GoReleaser writeup](https://goreleaser.com/blog/cosign-v3/)).

## Technology Landscape

- **Trending:** OCI 1.1 referrers adoption, fragmented by registry; cosign v3 bundle-as-referrer default; zot as reference implementation.
- **Established:** cosign v1/v2 sidecar-tag convention (`sha256-<hex>.sig/.att/.sbom`) as the still-necessary interoperability fallback — zot's new legacy-tag sync toggle keeps it alive.
- **Emerging:** dedicated concurrent OCI sync tools, e.g. [`ocync`](https://github.com/clowdhaus/ocync) — adoption unverified.
- **Declining:** none clearly; the tag-schema fallback stays load-bearing because ECR/GHCR/GitLab gaps keep the native API from being universal.

## Key Findings

1. Harbor's replication gap is a **tool defect, not a spec gap** — `oras copy --recursive` works between the same registries. [harbor#23210](https://github.com/goharbor/harbor/issues/23210)
2. The fallback-tag merge is explicitly non-atomic by the spec's own text; ETag conditioning is the only (optional) mitigation. [spec.md](https://github.com/opencontainers/distribution-spec/blob/main/spec.md)
3. cosign 2.x re-signing is not idempotent — an unconditional backfill sweep can hard-fail rather than skip. [cosign#3356](https://github.com/sigstore/cosign/issues/3356)
4. Multi-signature AND/OR is a verifier policy knob, not a spec guarantee. [Kyverno](https://kyverno.io/docs/policy-types/cluster-policy/verify-images/overview/)
5. ECR rejects `subject`-bearing manifests outright (405) — a destination can lack not just the API but the ability to store a referrer at all. [containers-roadmap#2783](https://github.com/aws/containers-roadmap/issues/2783)

## negative

- GAR Referrers-API status unconfirmed.
- Kyverno AND vs any-one semantics: two sources conflict; needs a primary-source read.
- No depth/cycle documentation for `oras cp --recursive`.
- No first-party backfill/sweep tool anywhere — "script your own" is the state of the art.
- Docker distribution v3 / Quay referrers status not resolved in this pass.

## leads

- `ocync` (clowdhaus) — possible faster registry-to-registry sync alternative; low adoption signal.
- Zot's `SyncLegacyCosignTags` + skip-already-synced pattern — a template for sidecar-vs-referrer duplication handling.
- ECR 405-on-`subject` (2026-03, open) — targeted test before relying on referrer pushes to ECR.

## Sources

| Source | Type | Date | Relevance |
|--------|------|------|-----------|
| https://github.com/opencontainers/distribution-spec/blob/main/spec.md | Spec | current | referrers API, tag-schema fallback, race warning |
| https://opencontainers.org/posts/blog/2024-03-13-image-and-distribution-1-1/ | Blog | 2024-03 | spec ship date |
| https://oras.land/docs/commands/oras_cp/ | Docs | current | `--recursive` (Preview) |
| https://github.com/sigstore/cosign/blob/main/doc/cosign_copy.md | Docs | current | `--only=sig,att,sbom` |
| https://regclient.org/usage/regsync/ | Docs | current | referrer sync opt-in |
| https://github.com/containers/skopeo/blob/main/docs/skopeo-copy.1.md | Docs | current | `--preserve-digests`, `--sign-by` |
| https://github.com/goharbor/harbor/issues/23210 | Issue | 2026 | replication drops referrers |
| https://github.com/goharbor/harbor/issues/17107 | Issue | older, flagged | earlier same-shape report |
| https://github.com/goharbor/harbor/issues/21636 | Issue | 2026 | replication fails on signed images |
| https://github.com/goharbor/harbor/issues/22592 | Issue | 2026 | synced signature typed UNKNOWN |
| https://github.com/goharbor/harbor/wiki/Harbor-FAQs | Wiki | current | pre-2.9.2 ordering bug |
| https://github.com/aws/containers-roadmap/issues/2783 | Issue | 2026-03 | ECR 405 on `subject` |
| https://github.com/sigstore/cosign/issues/4641 | Issue | current | referrers push without valid fallback tag |
| https://github.com/sigstore/cosign/issues/3356 | Issue | current | non-idempotent signing |
| https://kyverno.io/docs/policy-types/cluster-policy/verify-images/overview/ | Docs | current | `attestors.count` |
| https://docs.sigstore.dev/policy-controller/overview/ | Docs | current | identity-based trust |
| https://github.com/notaryproject/specifications/blob/main/specs/trust-store-trust-policy.md | Spec | current | multi-signer open question |
| https://blog.sigstore.dev/cosign-3-0-available/ | Blog | 2026 | bundle-format default |
| https://goreleaser.com/blog/cosign-v3/ | Blog | 2026 | cosign v3 behaviour |
| https://github.com/project-zot/zot/releases | Releases | recent | `SyncLegacyCosignTags` |
| https://github.com/orgs/community/discussions/163029 | Discussion | current | GHCR lacks Referrers API |
| https://github.com/clowdhaus/ocync | Repo | current | emerging sync tool |
