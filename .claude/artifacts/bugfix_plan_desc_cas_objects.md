# Bug Fix Plan: registry sync — README/logo CAS objects in the mirrored index tree

## Status

- **Plan:** bugfix_desc_cas_objects
- **Active phase:** 7 — Commit & Document
- **Step:** awaiting /finalize
- **Last update:** 2026-09-13 (fix, unit and acceptance tests, docs and contracts landed on `fix/desc-cas-objects`; red proof recorded below)

---

## Overview

**Status:** Approved
**Author:** Claude (planned with owner approval; implementation baseline handed over from a stopped agent, re-validated against the codebase)
**Date:** 2026-09-13
**GitHub Issue:** [#68](https://github.com/ocx-sh/ocx-mirror/issues/68) (report), [#69](https://github.com/ocx-sh/ocx-mirror/issues/69), [#70](https://github.com/ocx-sh/ocx-mirror/issues/70), [#71](https://github.com/ocx-sh/ocx-mirror/issues/71)
**Severity:** High

## Bug Report

### Observed Behavior

A `registry sync` output tree fails `ocx-catalog build` with `dangling CAS reference`: the mirrored
root's `desc.readme` / `desc.logo` digests name objects the tree does not hold. Two side findings
from the same RCA: a description-only change upstream is never re-synced (`should_skip` looks at
tags only), and once objects are written an updated description would leave the previous ones
behind forever.

### Expected Behavior

Every digest a mirrored root names resolves to a file under `p/<ns>/<pkg>/o/sha256/`; a new README
or logo upstream reaches the mirror on the next run; only the objects the current root names remain
(the `.json` image indices are tag history and stay).

### Reproduction Steps

1. Author a published-shape source tree whose package root carries `desc.readme` / `desc.logo` and
   serves the objects beside it (`test/src/static_index.py`, `description_objects`).
2. `ocx-mirror registry sync registry.yml`.
3. `ls <output>/<as>/p/<ns>/<pkg>/o/sha256/` — only `<hex>.json`; no `.md` / `.svg`.

### Environment

| Factor | Value |
|--------|-------|
| Platform | any |
| ocx-mirror version | ≤ 0.6.0 (`main` at `7154c40`) |
| Registry | any |
| Configuration | any `registry.yml` |

### Frequency

Always, for every package whose root carries a `desc`.

## Root Cause Analysis

### Investigation Log

1. **Symptom**: `ocx-catalog build` aborts on the mirrored tree with `dangling CAS reference`.
2. **Proximate cause**: `@ocx-sh/catalog` resolves `desc.readme` / `desc.logo` against the tree's
   `contentByDigest` and the digests are absent.
3. **Root cause**: `sync_package` copies `__ocx.desc` into the destination *registry*
   (`registry_copy::copy_description`) and never writes the README/logo bytes into the *tree*; the
   tree receives only the root (`desc.*` copied through verbatim by `merge_root_tags`) and the
   `.json` dispatch objects (`write_dispatch_objects`). `should_skip` then compares tags and the
   catalog entry only, so a later description change cannot trigger a re-sync either.
4. **Introduced by**: original implementation ([ocx-sh/ocx-mirror#54](https://github.com/ocx-sh/ocx-mirror/pull/54)) — the
   "`o/` is indices-only" assumption in C-033 was never true of the upstream tree.

### Root Cause Statement

> The mirrored root names README/logo digests because the merge copies `desc` verbatim, but nothing
> in the write path ever produces the objects those digests name, and nothing in the skip path ever
> notices `desc` changing — introduced with the original `registry sync` implementation.

### Related Code

| File | Lines | Role |
|------|-------|------|
| `src/pipeline/registry_sync.rs` | `sync_package`, `write_package` | Orchestration: fetch objects before a blob moves; write before the root; prune after the commit |
| `src/pipeline/registry_sync/catalog.rs` | `fetch_description_objects` | The tree-side fetch, digest-verified, two error classes |
| `src/pipeline/registry_sync/index_write.rs` | `write_description_objects`, `prune_description_objects`, `should_skip` | The tree writes and the fourth skip condition |

### Pattern Check

- [x] Searched similar code with same defect — the dispatch objects follow the same root-then-object
      invariant and were already covered (C-033); nothing else in the tree is referenced by digest.
- [x] Checked: regression from recent change? No — original implementation.
- [x] Checked: other callers affected by same root cause? `--repair-catalog` re-derives
      `c/index.json` only and never walks `o/` (`IndexStore::list_wire_repositories` collects `.json`
      items, never descends a package dir) — unaffected.

## Regression Test Specification

> Written before the fix; red on a `main`-built binary, green on the branch.

### Unit Tests

| Test | File | Asserts |
|------|------|---------|
| `the_readme_and_logo_come_back_verified_under_the_extension_the_tree_serves` (+8) | `src/pipeline/registry_sync/catalog/tests/description.rs` | Paths tried, bytes verified, `ExecutionFailed` for foreign faults vs `SourceError` for a read that did not answer |
| `the_objects_land_beside_the_dispatch_objects_under_their_own_extension` (+5) | `src/pipeline/registry_sync/index_write/tests/description.rs` | Write path, write-if-changed, prune keeps `.json` and what the root names |
| `a_description_only_change_is_not_skipped`, `a_rewritten_pointer_and_a_tag_superset_are_not_drift` | `src/pipeline/registry_sync/index_write/tests/skip.rs` | Condition 4 red/green |
| `a_description_update_replaces_the_readme_and_keeps_the_dispatch_object`, `an_unchanged_source_is_skipped_right_after_its_own_publish` | `src/pipeline/registry_sync/tests/publish.rs` | Outcome on a real store; the skip holds on the bytes a publish wrote |
| `the_package_write_follows_c030s_order_exactly`, `the_description_objects_are_fetched_before_a_blob_moves_and_only_a_refusal_fails_the_package` | `src/pipeline/registry_sync/tests.rs` | Structural order guards |

### Acceptance Tests

| Scenario | File | Steps |
|----------|------|-------|
| S-012 `test_the_package_description_travels` | `test/tests/test_registry_sync.py` | Real README + SVG in the source tree → both land byte-identical in the output tree |
| S-029 `test_a_description_only_change_is_resynced_and_the_old_objects_pruned` | same | README moves, no tag does → `1 copied`, old `.md` pruned, `.json` kept; a second package forces the fallback → `1 copied, 1 skipped` |
| S-030 `test_a_root_naming_a_description_object_the_tree_lacks_fails_that_package_only` | same | Dangling `desc` → exit 1, that package failed, the other copied, no root written, retried next run |

**Red proof (2026-09-13):** the three scenarios against a `main`-built binary
(`OCX_MIRROR_COMMAND=<worktree>/target/release/ocx-mirror`): 3 failed — "no description object at
…/o/sha256/<hex>.md" (S-012, S-029) and rc=0 with both packages `copied` (S-030). Same three against
the branch binary: 3 passed.

## Fix Approach

### Proposed Change

Fetch the README/logo the root names from the **source tree** (`p/<ns>/<pkg>/o/<algo>/<hex>.<ext>`,
`.md` / `.svg` / `.png`, digest-verified) before a blob moves; write them beside the dispatch objects
before the root; prune the non-`.json` objects the root no longer names after the commit; add a
fourth `should_skip` condition comparing every package-level field. Contract C-048 and the amended
C-032/C-033 in `plan_registry_mirror_sync.md` record the details.

**Departure from #69's text (update the issue when landing):** objects are fetched from the tree,
not rebuilt from the registry's `__ocx.desc` layers, so root and objects come from one source
snapshot. **Broader than #70's text:** condition 4 compares all package-level fields, not only
`desc.digest` — the merge copies all of them from the source, so any difference is an update the
tree lacks; `desc.digest` is a special case.

### Files to Modify

| File | Change |
|------|--------|
| `src/pipeline/registry_sync/catalog.rs` | `DescriptionObject`, `fetch_description_objects`, `cas_object_path`, `refused` |
| `src/pipeline/registry_sync/index_write.rs` | `write_description_objects`, `prune_description_objects`, `should_skip` condition 4 |
| `src/pipeline/registry_sync.rs` | Fetch before the tag loop with the C-040 split; write and prune in `write_package` |
| `test/src/static_index.py`, `test/tests/test_registry_sync.py` | Fixture objects, two assertions, S-012 rewrite, S-029, S-030 |
| `docs/reference/registry-yml.md`, `docs/reference/cli.md` | Tree layout, incremental section, exit-code rows |
| `.claude/rules/subsystem-mirror.md`, `.claude/state/plans/plan_registry_mirror_sync.md` | Module map, C-032/C-033/C-048, S-012/S-029/S-030 |

### Alternatives Considered

| Approach | Rejected Because |
|----------|-----------------|
| Write the bytes `copy_description` already pulled from the registry's `__ocx.desc` layers (#69's text) | Registry and index are two snapshots; a description pushed ahead of its index leaves the root naming digests the tree lacks — the same defect |
| Compare `desc.digest` only (#70's text) | Any package-level field the merge copies can move; the general comparison costs the same and subsumes it |
| A config option to skip the objects | Owner ruling: a tree referencing digests it does not hold is invalid under either setting |
| Prune `.json` objects yanked tags no longer reach (upstream's D8 does) | The append-only `tags{}` ruling (C-047) covers dispatch objects; out of scope by design |

### Risk Assessment

| Risk | Mitigation |
|------|------------|
| Foreign README/logo bytes republished under the operator's origin | Already the ADR's recorded residual for the registry copy; the tree copy is digest-verified and body-capped, sanitising is the renderer's job |
| A store-side field would defeat condition 4 and re-copy every package every run | `an_unchanged_source_is_skipped_right_after_its_own_publish` pins the skip on the bytes a publish wrote; S-029 run 3 pins it end to end |
| Prune removes something a root still names | Runs after the commit with the digests just written; `.json` never touched; S-029 and the publish test check both |
| A transport failure on the object fetch demoted to a package failure | Split by variant: only `ExecutionFailed` fails the package, everything else aborts (structural guard + unit test) |

## Verification Checklist

- [x] Regression test fail on current code (prove bug exist) — red proof above
- [x] Fix applied — regression test now pass — 3/3 acceptance, all new unit tests green
- [x] All existing tests still pass (`task verify`) — see Notes
- [x] Manual reproduction steps no longer reproduce bug — S-012 is the reproduction
- [x] No scope creep — the handover's diff plus the seven corrections the review found

## Notes

Baseline was an uncommitted diff plus a handover from a stopped agent; the review before tests found
and fixed: the C-040 class collapse on the object fetch, the fetch placed after the blob copy, a
prune walk that aborted the run on a stray file under `o/`, the stale "`o/` is indices-only"
wording, missing structural guards, and the stale `should_skip` signature in `plan.rs`'s doc.
Review (opus, correctness + security, threat-model scoped): one confirmed finding, fixed and
pinned by `a_nested_package_inside_the_object_directory_is_left_alone` — catalog keys nest
(`a/b` and `a/b/o/sha256/c` are both legal), so a sibling package's subtree can sit inside a
package's `o/<algo>/` and the prune must step over directories. One deferred: a static host that
answers **403** for a missing key (S3 without public `ListBucket`) turns the `.svg`-before-`.png`
logo probe into a run abort, because only a 404 reads as absence (the policy every index fetch
shares, pinned by `only_a_404_reads_as_absence`). Left as is — the upstream index is GitHub
Pages — and surfaced to the owner; if such hosts are in scope, a non-404 non-success during the
probe should fail the package rather than the run.
Landing sequence: one `fix(registry):` commit closing #68/#69/#70/#71 plus a `chore(claude):` for
the artifacts — not the three-way split the plan first proposed: the three fixes share hunks in
every touched file (S-029 exercises #70 and #71 in one run, the structural guard covers #69 and
#71 in one test), so per-issue commits would not compile or test in isolation. After landing: comment on #69 with the tree-fetch departure,
link the PR on #68.
