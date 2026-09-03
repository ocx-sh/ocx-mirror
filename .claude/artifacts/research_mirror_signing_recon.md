# Research: ocx-mirror signing surfaces (codebase recon)

<!-- hex-discuss research lane: codebase recon. Discussion: .agents/discussions/mirror-signing.md -->

## Metadata

**Date:** 2026-09-01
**Domain:** packaging | security
**Triggered by:** discussion `mirror-signing` — which signing/referrer primitives exist in ocx 0.6.0 vs must be built in the mirror, per leg.
**Expires:** next ocx submodule bump (surfaces cited by file:line at `v0.6.0`)

## Direct Answer

1. **`ocx package push --sign`** — `external/ocx/crates/ocx_cli/src/command/package_push.rs`: `ArgGroup signing_target = [sign, sbom]`; `signing_modifier = [signature_format, key, rekor_upload, no_rekor_upload]` requires a target. `--sign` signs each **platform manifest**; the index digest is rewritten on every platform merge, so the index is signed afterwards via `ocx package sign --tags-file`. Offline + `--sign` refuses with exit 77 before `Publisher::new`. Push carries no `--identity-token-*` flags (env/ambient only). Post-push sign failures are reported per platform in `PushReport`, not rolled back.

2. **`ocx package sign`** — `package_sign.rs`: sweep = `--tags`/`--tags-file`, `--platform` conflicts with both. `sign_tags` (`ocx_lib/src/package_manager/tasks/sign.rs:193`) yields `SweptOutcome::{SkippedBareManifest, Failed, Done}`; the only skip is "resolves to a single manifest, which push already signed". **No `--force`, no already-signed-by-identity skip**: each run appends another signature.

3. **Referrer attach** — `ocx_lib/src/oci/sign/referrers.rs:1-127`: `attach_referrer` = `push_referrer_manifest` (native) plus, only when `ReferrersSupport::Unsupported`, `append_referrer_fallback_index` (GET-merge-PUT inside the fork's `native::Client`). **`pub(crate)`** — no public seam to attach an externally sourced referrer manifest. Capability cache `$OCX_HOME/state/referrers/<registry>.json`. `ocx package copy` carries referrers (`options::Referrers`, default on, exit 84 without the API) with `sidecar_conflicts` on `CopyReport` — a different code path from the mirror's `registry_copy.rs`.

4. **Sidecar format** — `oci/simplesigning.rs` (payload; field order significant). `SignatureFormat::{Bundle, Simplesigning, Both}` selects referrer vs sidecar vs both. `sign/simplesigning_write.rs` not read.

5. **Mirror registry copy** — `src/pipeline/registry_copy.rs`: `detect_referrers` probes only the native API and the bare `sha256-<hex>` fallback tag; any hit fails the whole package (`CopyError::ReferrersPresent`; acceptance `test_a_package_carrying_a_referrer_fails_with_a_counted_error`, `test/tests/test_registry_sync.py:2083-2120`, S-011). Cosign `.sig`/`.att`/`.sbom` tags are **not** probed. `Tag::is_reserved` (`ocx_lib/src/package/tag.rs`) covers `Internal`, `LegacyKeep`, and the referrer-fallback shape only; sidecar suffixes (`tag.rs` `SIDECAR_SUFFIXES`) are `Tag::Other` and not reserved. Under the module's "filters nothing, every `tags{}` key travels by digest" doctrine, sidecar tags present in the source index are **copied through as ordinary tags — neither detected nor refused, and untested**.

6. **Env forwarding and push legs** — `src/pipeline/ocx_cli.rs:41-63` forwards 13 `OCX_*` vars, none of the four signing vars. Two push legs: in-process `ocx_lib::publisher::Publisher` via `src/pipeline/push.rs::push_and_cascade` (called from `src/pipeline/orchestrator.rs:850` — the github/url pipeline), and subprocess `src/pipeline/ocx_cli/push.rs::build_push_args` (used by `src/pipeline/python_push.rs:227`, `src/command/package/pipeline/push.rs:762`, `patch.rs:358`). Neither has a signing slot. `generate/ci/permissions.rs`: `GHCR_PUSH_PERMISSIONS`, `GHCR_DISCOVER_PERMISSIONS`, `GHCR_REGISTRY_WRITE_PERMISSIONS` — none carry `id-token: write`.

7. **Verify side** — `oci/verify/discovery.rs`: `DiscoveryMethod::{ReferrersApi, FallbackTag, SidecarTag}`; `verify/pipeline.rs` discovers bundle referrers, `.sig`/`.att` sidecars and `.sbom` unless `signature_format` narrows; `MAX_SIGNATURE_CANDIDATES = 8` (`pipeline.rs:91`), ANY-of. Trust policy schema (`[[trust.policy]]`) not read.

8. **Acceptance harness** — `test/docker-compose.yml`: `registry` and `mirror_registry` both `registry:2` (untagged minor), ports 5001/5002. Referrers-API support of that image not confirmed in-tree. Existing tests: `seed_referrer` (~2035), S-011 counted-error test, S-024 attestation-shaped index children (copied, not detected). No sidecar-tag copy test.

9. **Prior decisions** — `adr_registry_mirror_sync.md:580-639`: depth-1 detection, `MAX_INDEX_DOCUMENT_BYTES`, first-10 report; "when v2 copies referrers, the walk needs its own depth cap". Open question 1 (amended 2026-08-14): blob-copy seam is a direct `native::Client` construction in the mirror (Seam 2), chosen because "the PR must stand alone".

## Key Findings

1. Referrer refusal is binary and total: one detected referrer fails the whole package copy.
2. Sidecar tags are an unguarded pass-through, not a refusal — and untested.
3. No signing primitive reaches either push leg; the insertion point differs per leg (in-process `Publisher` vs subprocess argv).
4. Ambient GHA keyless signing is blocked by the rendered `permissions:` blocks lacking `id-token: write`.
5. `attach_referrer` is `pub(crate)`: attaching a *copied* referrer manifest needs either ocx's own copy flow or a new ocx_lib seam.
6. `ocx package copy` already carries referrers + sidecars — a built mechanism the mirror does not reuse.
7. The sign sweep is not idempotent across runs; with an 8-candidate verify cap, unconditional backfills accumulate.

## negative

- `registry:2` Referrers-API support unconfirmed.
- `sign/simplesigning_write.rs`, `PackageManager::sign_tags` internals beyond the skip rule, trust-policy schema, and `adr_registry_mirror_sync.md:796-835, 1215-1240` not read this pass.

## leads

- Read `simplesigning_write.rs` for sidecar write mechanics (single PUT vs merge).
- Confirm `registry:2`'s Referrers API before building acceptance tests on it.
