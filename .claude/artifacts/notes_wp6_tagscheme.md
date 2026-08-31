# WP6 — reserved-tag scheme and insecure-host adoption (ocx 0.6.0)

Verification notes. Branch work against `external/ocx` at `v0.6.0` (`e48ef73c`).
Companion to `research_ocx_060_semantics.md`, which raised three gaps it could
not execute. Two of the three are refuted here.

## A2 decision: the mirror keeps writing `sha256.<hex>`

`registry_copy::push_canonical_tag` deliberately keeps writing the frozen legacy
`<alg>.<hex>` keep-tag spelling and does **not** follow ocx 0.6.0's rename to
`__ocx.keep.<alg>-<hex>`. Four pieces of evidence, all checked against the v0.6.0
source rather than inferred. First, ocx documents `Tag::LegacyKeep` as a
permanent read arm — *"This is a read arm and nothing else… The arm stays because
already-published repositories carry these tags, and they must keep classifying
as reserved so they are never read back as a version"*
(`external/ocx/crates/ocx_lib/src/package/tag.rs:124-131`). Second,
`Tag::is_reserved()` returns `true` for both spellings, so both are filtered out
of index roots and version listings identically — there is no behavioural
divergence, only a spelling one. Third, a grep across `external/ocx/crates` finds
**zero read-side consumers** of the keep-tag spelling: it is written once
(`oci/client.rs:752`) and echoed in a JSON report field, and nothing anywhere
matches on its shape. Fourth, switching would add a *second* tag to every
manifest on every already-mirrored destination, because nothing in the mirror
deletes the old one — a permanent cleanup cost on live registries. A wire-format
change with no behavioural gain and a permanent cleanup cost is the wrong trade,
and "ocx renamed the concept" is not by itself a reason to rewrite published
tags. Only the three now-false doc claims were corrected
(`registry_copy.rs:999`, `spec/registry.rs:114`, `python_push.rs:42`).

## Do not delete `ocx_still_classifies_the_legacy_keep_tag_spelling_as_reserved`

That test (`src/command/package/pipeline/push/tests/ordering.rs`) is the
load-bearing artefact of the decision above, not a redundant assertion. The whole
A2 argument rests on one cross-binary assumption — that `ocx_lib` still
classifies the spelling this mirror writes as reserved. The test converts that
assumption into something a future `external/ocx` submodule bump breaks loudly.
It cannot fail against v0.6.0 and is not supposed to; it exists so that the day
someone drops the `Tag::LegacyKeep` read arm, the mirror's own deletion
safety-net tags do not start reading back as versions at every consumer in
silence. Verified to discriminate: mutating its subject to a short hex produced
`FAILED … panicked at ordering.rs:56`.

## Second fake-green in this branch

The pre-existing ordering fixture used `"sha256.abc123"`. That hex is 6
characters, so `parse_keep` rejects it outright — it was never a valid keep tag,
was never classified as reserved, and had never been able to fail. It pinned
nothing. Replaced with real 64-hex shapes covering both keep-tag spellings plus
the OCI referrers and cosign sidecar forms. This is the second fake-green found
in this branch.

Related and worth keeping in mind: the swap of the hand-rolled `sha256.` prefix
filter for `Tag::is_reserved_str` at `alias.rs:162` is anti-drift hardening, not
a bug fix. No red is achievable through `registry_tag_newer_than` — its
`key.0.is_some()` guard already excludes every reserved shape, since all of them
parse to `None` under `pep440_sort_key`. The new fixture was run against the old
filter and passed.

## Open items for the owner

- Decide whether `.claude/artifacts/research_ocx_060_semantics.md` should be
  corrected or annotated. Its items 1, 3 and 9a assert runtime defects that do
  not exist, and another agent reading it will re-attempt all three.
- Confirm the intended branch. This work was done on `main` with an uncommitted
  tree, not on `feat/ocx-0.6-adoption` as the WP6 brief stated.
