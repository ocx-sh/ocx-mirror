# Research: verification after digest-preserving relocation

<!-- hex-discuss disputed-fact check. Discussion: .agents/discussions/mirror-signing.md -->

## Metadata

**Date:** 2026-09-01
**Domain:** security
**Triggered by:** discussion `mirror-signing` — the SOTA lane claimed cosign verify fails by default on a relocated artifact; `research_mirror_supply_chain.md` §3(a) said a digest-preserving copy keeps the signature valid.
**Expires:** 2027-03-01 (re-check against cosign v3's bundle verify path)

## Direct Answer

A digest-preserving copy into a different repository or registry **verifies by default** under cosign and under ocx: both compare only `critical.image.docker-manifest-digest` against the subject digest and never read `critical.identity.docker-reference`. The cited failure ([cosign#2790](https://github.com/sigstore/cosign/issues/2790)) was a raw ASN.1 signature error from a `docker save`/`load`/`push` workflow that re-serialises the manifest — not a reference mismatch. The "relocation breaks verification" belief most plausibly comes from skopeo's `containers-policy.json`, whose default `signedIdentity: matchRepoDigestOrExact` **does** reject a repository-relocated image — a policy mechanism cosign and ocx do not share.

## Key Findings

1. Payload spec ([containers-signature.5.md](https://github.com/containers/image/blob/main/docs/containers-signature.5.md)): `critical.image.docker-manifest-digest` must match; the spec says verifiers "MUST confirm" `docker-reference` — normative on paper, not what cosign's shipped verifier does.
2. cosign default check ([`pkg/cosign/verifiers.go`](https://github.com/sigstore/cosign/blob/main/pkg/cosign/verifiers.go) `SimpleClaimVerifier`, on by `--check-claims=true`): compares `Critical.Image.DockerManifestDigest` to the image digest only; `docker-reference` is never read.
3. [cosign#2790](https://github.com/sigstore/cosign/issues/2790): error is `no matching signatures: invalid signature when validating ASN.1 encoded signature` — crypto failure before any claim check; reproduction did not preserve digests. Maintainer thread not retrievable via fetch.
4. Relocation fails only when the copy changes the digest (re-serialisation), when the signature artifact (sidecar tag or referrer) is not carried, or when tlog/SET/cert-chain evidence is unavailable offline at the destination.
5. ocx's verifier `external/ocx/crates/ocx_lib/src/oci/verify/simplesigning_read.rs:881-895` (`check_claim`) parses `docker_reference` (`simplesigning.rs:53-54`) but compares only `docker_manifest_digest` → `VerifyErrorKind::SubjectDigestMismatch`; `rg docker_reference` under `oci/verify/` has no comparison site.
6. skopeo ([containers-policy.json.5.md](https://github.com/containers/image/blob/main/docs/containers-policy.json.5.md)): `matchRepoDigestOrExact` default — digest references require the *same repository*; a relocated image is rejected under skopeo policy enforcement even with a matching digest.

## negative

- cosign v3 bundle-verify call site not independently pulled (image signatures share the simplesigning `messageSignature` payload per the bundle spec; assumed unchanged).
- cosign#2790 maintainer diagnosis not retrievable.
- Whether referrers-API-discovered signatures take a different claim-check path in current cosign was not checked.

## leads

- Confirm cosign v3 `cmd/cosign/cli/verify.go` claim-check site for image signatures.
- Docs note for the mirror: "copies preserved" verifies under cosign/ocx; skopeo/podman default policy needs `signedIdentity` relaxed (`matchRepository` remap or `exactRepository`) for mirrored repositories.
