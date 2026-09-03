# Research: signature backfill, identity pre-filter, and multi-signature semantics (2026)

<!-- hex-plan research axis: package-manager supply chain. Plan: mirror-signing -->

## Metadata

**Date:** 2026-09-02
**Domain:** security | packaging
**Triggered by:** plan `mirror-signing` — the backfill pre-filter's matching rule, whether a no-crypto listing suffices, `--force`/rotation semantics, sweep failure policy.
**Expires:** 2027-03-01 (cosign referrers-API coverage and Rekor v2 rollout move quickly)

## Direct Answer

1. **Keyless identity fields; is an unvalidated read a sound skip test?** Fulcio puts identity in the cert SAN and the OIDC issuer under `1.3.6.1.4.1.57264.1.*` ([Fulcio OID reference](https://github.com/sigstore/fulcio/blob/main/docs/oid-info.md), [OIDC in Fulcio](https://docs.sigstore.dev/certificate_authority/oidc-in-fulcio/)); `cosign verify --certificate-identity --certificate-oidc-issuer` is the match primitive ([Verifying](https://docs.sigstore.dev/cosign/verifying/verify/)). cosign's signature spec requires chain validation before trusting any cert field ([SIGNATURE_SPEC.md](https://github.com/sigstore/cosign/blob/main/specs/SIGNATURE_SPEC.md)). An actor with repo write access can attach a self-signed cert claiming any SAN/issuer and force a skip. Chain validation (Fulcio root, SCT, validity window) without a Rekor round-trip is the cheap closing move. **Threat-model note (lead repo):** `security-threat-model.md` trusts whoever can write to the destination registry, so this attacker is out of scope by owner ruling — the design records the choice either way.
2. **Key-pair identity.** The bundle's `verificationMaterial.publicKey.hint` is the SHA-256 of the DER public key ([Bundle Format](https://docs.sigstore.dev/about/bundle/), [cosign verify.go](https://github.com/sigstore/cosign/blob/main/pkg/cosign/verify.go)); comparing the configured key's hint is a sound no-crypto pre-filter — a forged hint only makes a signature that then fails verification.
3. **Sweep tooling and idempotency.** No purpose-built "skip if signed by X" tooling; GitHub attestation bulk management is "planned" ([Artifact Attestations](https://docs.github.com/en/actions/concepts/security/artifact-attestations)). cosign 2.x tlog re-sign non-idempotency ([cosign#3356](https://github.com/sigstore/cosign/issues/3356)) was fixed via PR #3371 — release cutoff not stated; pin a floor. Sweeps in the wild are `oras attach`/`cosign sign` loops; ORAS ships `discover`/`manifest fetch` only ([ORAS commands](https://oras.land/docs/category/oras-commands/)).
4. **Rotation.** Kyverno `verifyImages.attestors.count: 1` over old+new entries is the documented "either A or B" ([Kyverno verify-images](https://kyverno.io/docs/policy-types/cluster-policy/verify-images/)); policy-controller multi-authority lists likewise. No tool revokes old signatures; practice is additive with a transition window.
5. **Many signatures per subject.** No cap in the Referrers API or cosign; pagination (`n`) and `artifactType` filtering are required, page size registry-specific ([manifest-referrers-api.md](https://github.com/oras-project/artifacts-spec/blob/main/manifest-referrers-api.md)). cosign's own referrers support is incomplete (`download`, `verify-attestation`, `attach` still tag-based as of 2025-08, [cosign#4335](https://github.com/sigstore/cosign/issues/4335)) — discovery must use the raw referrers API with pagination, never `cosign download`.
6. **Rekor.** Rekor v2 GA 2025-10 batches uploads (higher QPS, seconds of added latency; drops v1 log-index/leaf-hash APIs) ([Rekor v2 GA](https://blog.sigstore.dev/rekor-v2-ga/)). `--offline` fails closed. No published public-Rekor rate limit; assume backoff/retry.

## Technology Landscape

- **Established:** keyless default; `count`-style multi-authority policies for rotation.
- **Trending:** referrers API replacing sidecar tags, unevenly (cosign itself lags).
- **Emerging:** Rekor v2 tile-based log; v1 entry semantics break.
- **Declining:** raw Rekor-bypass flags in mainstream workflows.

## Key Findings

1. Keyless pre-filter by SAN/issuer is spoofable without chain validation; key-hint pre-filter is self-authenticating. [SIGNATURE_SPEC.md](https://github.com/sigstore/cosign/blob/main/specs/SIGNATURE_SPEC.md), [Bundle Format](https://docs.sigstore.dev/about/bundle/)
2. cosign re-sign idempotency is fixed upstream; pin a floor. [cosign#3356](https://github.com/sigstore/cosign/issues/3356)
3. Rotation is additive; verifiers express "either" via count/threshold. [Kyverno](https://kyverno.io/docs/policy-types/cluster-policy/verify-images/)
4. No signatures-per-subject cap exists anywhere; ocx's 8-candidate cap is ocx's own. [referrers API](https://github.com/oras-project/artifacts-spec/blob/main/manifest-referrers-api.md)
5. Discovery must be raw referrers API + sidecar scan with pagination. [cosign#4335](https://github.com/sigstore/cosign/issues/4335)

## Recommendation

Keyless pre-filter: chain-validate (Fulcio root, SCT, validity window; no Rekor) before comparing SAN/issuer — or, under the lead's threat model, accept the listing read and record the residual. Key mode: compare the public-key hint with no crypto. `--force` re-signs safely on current cosign/ocx; pin the ocx floor. Rotation: additive, document a transition window and the "either identity" policy shape. Sweep failure: fail closed per subject with backoff, never abort the whole sweep on one transient; discover through the raw referrers API with pagination plus the three sidecar tags.

## Sources

| Source | Type | Date | Relevance |
|--------|------|------|-----------|
| https://github.com/sigstore/fulcio/blob/main/docs/oid-info.md | Docs | current | identity OIDs |
| https://docs.sigstore.dev/certificate_authority/oidc-in-fulcio/ | Docs | current | SAN/issuer |
| https://docs.sigstore.dev/cosign/verifying/verify/ | Docs | current | identity flags |
| https://github.com/sigstore/cosign/blob/main/specs/SIGNATURE_SPEC.md | Spec | current | chain validation required |
| https://docs.sigstore.dev/about/bundle/ | Docs | current | key hint |
| https://github.com/sigstore/cosign/blob/main/pkg/cosign/verify.go | Source | current | hint computation |
| https://docs.github.com/en/actions/concepts/security/artifact-attestations | Docs | current | no bulk tooling |
| https://github.com/sigstore/cosign/issues/3356 | Issue | 2023-11, fixed | idempotency |
| https://oras.land/docs/category/oras-commands/ | Docs | current | discover/fetch |
| https://kyverno.io/docs/policy-types/cluster-policy/verify-images/ | Docs | current | `count: 1` |
| https://github.com/oras-project/artifacts-spec/blob/main/manifest-referrers-api.md | Spec | flagged, older | pagination, filter |
| https://github.com/sigstore/cosign/issues/4335 | Issue | 2025-08 | incomplete referrers support |
| https://blog.sigstore.dev/rekor-v2-ga/ | Blog | 2025-10 | Rekor v2 |
