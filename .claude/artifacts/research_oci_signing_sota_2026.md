# Research: OCI signing and referrers — 2026 state of the art

<!-- hex-discuss research lane: SOTA. Discussion: .agents/discussions/mirror-signing.md -->

## Metadata

**Date:** 2026-09-01
**Domain:** packaging | security
**Triggered by:** discussion `mirror-signing` — which signature form ocx-mirror should produce for its own pushes and which forms it must carry through registry copies.
**Expires:** 2027-03-01 (cosign defaults and registry Referrers-API support move quarterly)

## Direct Answer

**1. Signature formats.** cosign v3 (2026) defaults to the Sigstore bundle (`application/vnd.dev.sigstore.bundle.v0.3+json`) stored as an OCI 1.1 referrer via `subject`. [Cosign v3 blog](https://blog.sigstore.dev/cosign-3-0-available/), [v3.0.1 release](https://github.com/sigstore/cosign/releases/tag/v3.0.1). The legacy simplesigning sidecar (`sha256-<hex>.sig`, `application/vnd.dev.cosign.simplesigning.v1+json`) is still read — verify auto-detects — with removal deferred to cosign v4, no date: [deprecation-notices #4696](https://github.com/sigstore/cosign/issues/4696). Notation ≥1.2 tries the Referrers API first and auto-falls-back to the tag schema: [notation-cli spec](https://github.com/notaryproject/notation/blob/main/specs/notation-cli.md).

**2. Registry support (2026 snapshot).** Native Referrers API: zot [deepwiki](https://deepwiki.com/project-zot/zot), Harbor, Quay.io [Red Hat blog](https://www.redhat.com/en/blog/announcing-open-container-initiativereferrers-api-quayio-step-towards-enhanced-security-and-compliance), Amazon ECR [AWS blog](https://aws.amazon.com/blogs/opensource/diving-into-oci-image-and-distribution-1-1-support-in-amazon-ecr/) (but see the prior-art artifact: 405 on `subject` pushes reported 2026-03, open), Azure ACR (except CMK-encrypted registries → tag schema) [MS Community Hub](https://techcommunity.microsoft.com/blog/appsonazureblog/announcing-support-of-oci-v1-1-specification-in-azure-container-registry/4177906), JFrog Artifactory ≥7.90.1 [JFrog docs](https://jfrog.com/help/r/jfrog-artifactory-documentation/use-referrers-rest-api-to-discover-oci-references). **Gaps:** GitLab container registry stores `subject` but serves no `/referrers` endpoint (404 on EE 18.x, 2026-02) and rejects dangling subject references — the referrer must be pushed after its subject exists [GitLab forum](https://forum.gitlab.com/t/i-m-running-gitlab-ee-currently-18-x-with-the-built-in-container-registry-i-m-trying-to-use-oci-1-1-artifacts-via-the-referrers-ap/132731). GHCR: no Referrers API [GitHub discussion #163029](https://github.com/orgs/community/discussions/163029). Docker Hub, Google Artifact Registry, Nexus: no confirmed 2026 status.

**3. Verifier behaviour.** cosign v3.1.x auto-detects bundle vs sidecar on verify [GoReleaser writeup](https://goreleaser.com/blog/cosign-v3/). Kyverno `verifyImages` can rewrite the registry reference before verifying and defaults `verifyDigest=true`; an August 2026 piece catalogs how a mirrored image can resolve a different digest, creds, signature repo, or narrower identity than local `cosign verify` [Kyverno post](https://oneuptime.com/blog/post/2026-08-11-kyverno-verifyimages-blocks-signed-images/view). **Claimed, disputed (see negative):** cosign verify fails by default when an image plus signature are copied to a new registry, with `--check-claims=false` the only workaround [cosign #2790](https://github.com/sigstore/cosign/issues/2790). Package-manager side: Homebrew `attestation.rb` behind `HOMEBREW_VERIFY_ATTESTATIONS` (default off) [Trail of Bits](https://blog.trailofbits.com/2024/05/14/a-peek-into-build-provenance-for-homebrew/), [rubydoc](https://rubydoc.brew.sh/Homebrew/Attestation.html); npm auto-generates Sigstore provenance on Trusted-Publishing publishes [npm docs](https://docs.npmjs.com/trusted-publishers/); PyPI PEP 740 attestations; crates.io Trusted Publishing GA, signing (RFC #3403) still proposal-stage [zenn.dev survey](https://zenn.dev/sqer/articles/e4df3d397f5651?locale=en).

**4. Keyless vs key-pair outside GitHub Actions.** Any OIDC-issuing CI works with Fulcio — GitLab CI ≥16, CircleCI, Buildkite, Google Cloud Build [systemshardening.com](https://www.systemshardening.com/articles/cicd/sigstore-keyless-signing/). Jenkins unconfirmed (no native ambient id-token without a plugin). Self-hosted Fulcio/Rekor exists, GA/v1.0, still "nontrivial" [Linnemann Labs](https://linnemanlabs.com/posts/self-hosted-sigstore-transparency-infrastructure/); no adoption numbers. Key-pair/KMS signing for air-gapped or non-OIDC CI is architecturally implied, not evidenced by a 2026 source.

**5. Emerging.** distribution-spec milestones (live): v1.1.2 (patch), v1.2.0 — `OCI-Referrers` header on manifest pull (#454/#463), registries rejecting manifests with non-existent subjects (#459, would formalise GitLab's push-order rule), `PUT /referrers` (#515) — and v2.0.0 (no date). OCI "[Distribution Spec Conformance Redesign](https://opencontainers.org/posts/blog/2026-04-04-distribution-spec-conformance/)" (2026-04). in-toto remains the SLSA envelope; BuildKit attestation manifests become referrers-discoverable only with `oci-artifact=true`.

## Technology Landscape

- **Trending:** Sigstore bundle-as-referrer (cosign v3 default); OIDC Trusted Publishing plus auto-attestation on npm/PyPI/crates.io; native Referrers API landing on Harbor, Quay, ECR, ACR, Artifactory within two years.
- **Established:** keyless signing via ambient OIDC across CI platforms beyond GitHub Actions.
- **Emerging:** distribution-spec 1.2 header/PUT/dangling-subject work; ORAS v1.3.0 referrers-aware backup/restore ([CNCF blog](https://www.cncf.io/blog/2025/10/06/announcing-oras-v1-3-0-elevating-artifact-and-registry-management-workflows/)); Homebrew-style GitHub-attestation verification.
- **Declining:** cosign simplesigning sidecar tag (still read, deprecated, no removal date).

## Key Findings

1. cosign v3 writes bundles as referrers by default and still reads sidecars; sidecar removal is unscheduled. [#4696](https://github.com/sigstore/cosign/issues/4696)
2. GitLab and GHCR — two of the most-used registries — lack the Referrers endpoint in 2026 while emitting `subject`-bearing manifests; the tag-schema fallback is fleet-wide load-bearing.
3. GitLab rejects a referrer whose subject does not yet exist: **push order (subject before referrer) is a live constraint**, and distribution-spec #459 may make it normative.
4. Verifiers are mirror-aware only by configuration (Kyverno reference rewrite, identity scoping); relocation is where verification breaks in practice.
5. JFrog returns 403 on `/referrers` without subject read access — referrer discovery is not guaranteed anonymous even where the API exists.

## negative

- **Disputed:** "cosign verify fails by default on relocation because the payload embeds the original digest" ([#2790](https://github.com/sigstore/cosign/issues/2790)) conflicts with `research_mirror_supply_chain.md` §3(a) ("a cosign signature is over the manifest digest, not a registry location, so a digest-preserving copy keeps it valid"). A digest-preserving copy leaves the digest claim intact; the simplesigning payload also carries `docker-reference` (the upstream repo path), which a mirror rewrite changes. Which claim cosign / ocx enforce by default is being fact-checked separately.
- Docker Hub, GAR, Nexus referrers status unconfirmed for 2026.
- ECR: SOTA sources say native support; prior-art artifact records a live 405 on `subject` pushes (2026-03). Both may be true; treat ECR as untested.
- No adoption numbers for self-hosted Sigstore or non-GitHub keyless CI.
- npm provenance adoption figure (76/1059) is one blog's sample.
- Jenkins ambient OIDC → Fulcio unconfirmed.

## leads

- ORAS v1.3.0 `oras cp -r` / backup-restore as the closest reference implementation for referrer-preserving mirroring — check whether oras-go copy semantics are worth mirroring in ocx_lib rather than re-deriving.
- cosign relocation behaviour (#2790) → a design note on what a "signature-preserving mirror" can and cannot promise.
- The Kyverno August 2026 post as a source for a signature-aware-mirroring compat/threat-model doc.
- distribution-spec #459 (reject dangling subjects) would make subject-before-referrer push order a spec requirement.
- JFrog `/referrers` 403 hardening: referrer discovery needs the same credentials as the subject.

## Sources

| Source | Type | Date | Relevance |
|--------|------|------|-----------|
| https://blog.sigstore.dev/cosign-3-0-available/ | Blog | 2026 | bundle default |
| https://github.com/sigstore/cosign/releases/tag/v3.0.1 | Release | 2026 | `--bundle` required |
| https://github.com/sigstore/cosign/issues/4696 | Issue | current | sidecar deprecation, removal in v4 |
| https://github.com/notaryproject/notation/blob/main/specs/notation-cli.md | Spec | current | Notation referrers fallback |
| https://deepwiki.com/project-zot/zot | Docs | current | zot 1.1.1 tracking |
| https://www.redhat.com/en/blog/announcing-open-container-initiativereferrers-api-quayio-step-towards-enhanced-security-and-compliance | Blog | flagged, >18mo? | Quay referrers |
| https://aws.amazon.com/blogs/opensource/diving-into-oci-image-and-distribution-1-1-support-in-amazon-ecr/ | Blog | flagged | ECR 1.1 support |
| https://techcommunity.microsoft.com/blog/appsonazureblog/announcing-support-of-oci-v1-1-specification-in-azure-container-registry/4177906 | Blog | flagged | ACR 1.1, CMK exception |
| https://jfrog.com/help/r/jfrog-artifactory-documentation/use-referrers-rest-api-to-discover-oci-references | Docs | current | Artifactory ≥7.90.1 |
| https://forum.gitlab.com/t/i-m-running-gitlab-ee-currently-18-x-with-the-built-in-container-registry-i-m-trying-to-use-oci-1-1-artifacts-via-the-referrers-ap/132731 | Forum | 2026-02 | GitLab no endpoint, dangling-subject rejection |
| https://github.com/orgs/community/discussions/163029 | Discussion | current | GHCR no Referrers API |
| https://goreleaser.com/blog/cosign-v3/ | Blog | 2026 | verify auto-detect |
| https://oneuptime.com/blog/post/2026-08-11-kyverno-verifyimages-blocks-signed-images/view | Blog | 2026-08 | mirror failure modes |
| https://github.com/sigstore/cosign/issues/2790 | Issue | 2023, flagged | relocation verify failure (disputed) |
| https://blog.trailofbits.com/2024/05/14/a-peek-into-build-provenance-for-homebrew/ | Blog | 2024-05, flagged | Homebrew attestations |
| https://rubydoc.brew.sh/Homebrew/Attestation.html | Docs | current | `HOMEBREW_VERIFY_ATTESTATIONS` |
| https://docs.npmjs.com/trusted-publishers/ | Docs | current | npm provenance |
| https://zenn.dev/sqer/articles/e4df3d397f5651?locale=en | Blog | 2026 | crates.io signing status |
| https://www.systemshardening.com/articles/cicd/sigstore-keyless-signing/ | Blog | current | keyless CI coverage |
| https://linnemanlabs.com/posts/self-hosted-sigstore-transparency-infrastructure/ | Blog | current | self-hosted Sigstore |
| https://opencontainers.org/posts/blog/2026-04-04-distribution-spec-conformance/ | Blog | 2026-04 | conformance redesign |
| https://www.cncf.io/blog/2025/10/06/announcing-oras-v1-3-0-elevating-artifact-and-registry-management-workflows/ | Blog | 2025-10 | ORAS 1.3 backup/restore |
