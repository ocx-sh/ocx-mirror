# Research: CI-rendered signing — keyless default, key fallback, self-hosted Sigstore (2026)

<!-- hex-plan research axis: registry ecosystems / package-manager supply chain. Plan: mirror-signing -->

## Metadata

**Date:** 2026-09-02
**Domain:** ci-cd | security
**Triggered by:** plan `mirror-signing` — the shape of the `sign:` spec block, the rendered-workflow contract, and how self-hosted Sigstore is supplied.
**Expires:** 2027-03-01 (GitHub OIDC audience-constraint work may change the rendered contract)

## Direct Answer

1. **Config-block precedents.** goreleaser `signs:`/`docker_signs:`: per-artifact opt-in blocks, `cmd: cosign`, templated `args` (`--bundle` keyless; `--key env://COSIGN_PRIVATE_KEY` key mode), `env:` passes secret **names** only ([goreleaser sign](https://goreleaser.com/customization/sign/sign/), [cosign v3 blog](https://goreleaser.com/blog/cosign-v3/)). apko/melange: `sign-with-temporary-key: true` is the keyless toggle; verification takes `certificate-identity` + `certificate-oidc-issuer` ([Chainguard guide](https://edu.chainguard.dev/open-source/sigstore/cosign/how-to-verify-file-signatures-with-cosign/)). Common shape: mode is a field; keyless declares no key material; key mode names an env var, never a value.
2. **GitHub Actions keyless.** `permissions: id-token: write` (+ `contents: read`; `packages: write` for GHCR; `attestations: write` only for `actions/attest-build-provenance`) ([attest-build-provenance](https://github.com/actions/attest-build-provenance)). Audience `sigstore`, obtained via `ACTIONS_ID_TOKEN_REQUEST_URL/_TOKEN`; cosign/sigstore-python detect GitHub, GitLab, Google, Buildkite, CircleCI ambiently ([Sigstore CI quickstart](https://docs.sigstore.dev/quickstart/quickstart-ci/), [sigstore-python#31](https://github.com/sigstore/sigstore-python/issues/31)). Fork `pull_request` runs get no token — gate signing to `push`/tag/`workflow_dispatch`/schedule ([community #137761](https://github.com/orgs/community/discussions/137761)). Fresh risk (2026-08-10): GitHub lets a job mint any audience at runtime; identity binding (repo+ref+workflow) is the mitigation ([yossarian.net](https://blog.yossarian.net/2026/08/10/github-actions-needs-oidc-audience-constraints)).
3. **Beyond GitHub.** GitLab CI `id_tokens: SIGSTORE_ID_TOKEN: aud: sigstore` — cosign auto-detects ([GitLab signing examples](https://docs.gitlab.com/ci/yaml/signing_examples/)). GHES issuer `https://HOSTNAME/_services/token`; public Fulcio trusts a fixed issuer allowlist, so GHES needs private Fulcio/Rekor ([GHES OIDC](https://docs.github.com/en/enterprise-server@3.17/actions/reference/security/oidc), [Fulcio oidc.md](https://github.com/sigstore/fulcio/blob/main/docs/oidc.md)). Jenkins: `oidc-provider-plugin` issues `aud: sigstore` JWTs ([plugin README](https://github.com/jenkinsci/oidc-provider-plugin/blob/master/README.md)). Cron box: key mode only. Key-mode guidance: `COSIGN_PRIVATE_KEY` + `COSIGN_PASSWORD`, prefer KMS via `env://`/`--kms` for masking and rotation ([key management](https://docs.sigstore.dev/cosign/key_management/signing_with_self-managed_keys/)).
4. **Self-hosted Sigstore.** `cosign trusted-root create` → one trusted-root file; `--trusted-root` on sign and verify; distribution via an org TUF repo. Beyond the root and `--fulcio-url`/`--rekor-url` (or `SIGSTORE_*` env), nothing extra from CI ([custom components](https://docs.sigstore.dev/cosign/system_config/custom_components/), [BYO sTUF](https://blog.sigstore.dev/sigstore-bring-your-own-stuf-with-tuf-40febfd2badd/)).
5. **Multi-arch.** `cosign sign --recursive` signs the index and every platform manifest; practitioner consensus is both, because a puller fetching one platform manifest fails if only the index is signed ([cosign_sign.md](https://github.com/sigstore/cosign/blob/main/doc/cosign_sign.md), [multi-arch writeup](https://some-natalie.dev/blog/sigstore-multiarch/)).
6. **Default-on vs opt-in; mirrors.** goreleaser, apko/melange, Notation action: opt-in. No mirror-shaped tool signs mirrored content with its own identity — aqua and mise *verify* upstream cosign/SLSA/minisign signatures; Harbor/Artifactory proxy caches sign nothing ([aqua cosign/SLSA](https://aquaproj.github.io/docs/reference/security/cosign-slsa/), [mise security](https://mise.jdx.dev/security.html)).

## Technology Landscape

- **Established:** keyless-by-default in CI; `--recursive` for multi-arch; opt-in per-artifact config blocks.
- **Trending:** `actions/attest-build-provenance` as the box-tick alternative to hand-rolled cosign steps.
- **Emerging:** statically scoped OIDC audiences (GitLab's model) as the fix for GitHub's runtime-audience gap.
- **Declining:** long-lived key-pair signing where ambient OIDC exists; PGP detached signatures.

## Key Findings

1. Every precedent names secrets by env var, never by value; mode is a field. [goreleaser](https://goreleaser.com/customization/sign/sign/)
2. Fork PRs never get an OIDC token; signing steps must be trigger-gated. [#137761](https://github.com/orgs/community/discussions/137761)
3. GHES on-prem issuers are not on public Fulcio's allowlist — private Sigstore is required there. [Fulcio oidc.md](https://github.com/sigstore/fulcio/blob/main/docs/oidc.md)
4. Sign index and platform manifests both. [cosign_sign.md](https://github.com/sigstore/cosign/blob/main/doc/cosign_sign.md)
5. No mirror tool re-signs mirrored content; verification is the norm. [mise security](https://mise.jdx.dev/security.html)

## Recommendation

`sign:` block modelled on goreleaser/apko: a mode field (`keyless` default, `key` fallback) with env-var names only; rendered GitHub workflow always emits `id-token: write` plus the narrowest extra permission, sign step gated to non-fork triggers. Self-hosted Sigstore = three passthrough fields (`fulcio_url`, `rekor_url`, `trusted_root`) or ocx config — the mirror never models the trust root. Non-GitHub targets: document the four-line job (GitLab `id_tokens:`, Jenkins OIDC plugin, key mode for a cron box). Sign both index and per-platform manifests.

## Sources

| Source | Type | Date | Relevance |
|--------|------|------|-----------|
| https://goreleaser.com/customization/sign/sign/ | Docs | current | `signs:` block shape |
| https://goreleaser.com/blog/cosign-v3/ | Blog | 2026 | cosign v3 args |
| https://edu.chainguard.dev/open-source/sigstore/cosign/how-to-verify-file-signatures-with-cosign/ | Docs | current | apko/melange keyless toggle |
| https://github.com/actions/attest-build-provenance | Repo | current | permissions |
| https://docs.sigstore.dev/quickstart/quickstart-ci/ | Docs | current | ambient OIDC |
| https://github.com/sigstore/sigstore-python/issues/31 | Issue | flagged, older | ambient detection list |
| https://github.com/orgs/community/discussions/137761 | Discussion | current | fork PRs no token |
| https://blog.yossarian.net/2026/08/10/github-actions-needs-oidc-audience-constraints | Blog | 2026-08 | runtime audience risk |
| https://docs.gitlab.com/ci/yaml/signing_examples/ | Docs | current | `id_tokens:` |
| https://docs.github.com/en/enterprise-server@3.17/actions/reference/security/oidc | Docs | current | GHES issuer |
| https://github.com/sigstore/fulcio/blob/main/docs/oidc.md | Docs | current | issuer allowlist |
| https://github.com/jenkinsci/oidc-provider-plugin/blob/master/README.md | Docs | current | Jenkins OIDC |
| https://docs.sigstore.dev/cosign/key_management/signing_with_self-managed_keys/ | Docs | current | key mode |
| https://docs.sigstore.dev/cosign/system_config/custom_components/ | Docs | current | self-hosted components |
| https://blog.sigstore.dev/sigstore-bring-your-own-stuf-with-tuf-40febfd2badd/ | Blog | flagged, older | TUF root distribution |
| https://github.com/sigstore/cosign/blob/main/doc/cosign_sign.md | Docs | current | `--recursive` |
| https://some-natalie.dev/blog/sigstore-multiarch/ | Blog | flagged, older | multi-arch practice |
| https://aquaproj.github.io/docs/reference/security/cosign-slsa/ | Docs | current | aqua verifies |
| https://mise.jdx.dev/security.html | Docs | current | mise verifies |
