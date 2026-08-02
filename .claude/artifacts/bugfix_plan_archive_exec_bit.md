# Bug Fix Plan: archive mirrors publish declared binaries without an exec bit

## Status

- **Plan:** bugfix_archive_exec_bit
- **Active phase:** 7 — Commit & Document
- **Step:** awaiting /finalize
- **Last update:** 2026-08-02 (after 853ab44: fix(pipeline): make declared binaries executable in archive bundles)

---

## Overview

**Status:** Approved
**Author:** Claude (planned with owner approval)
**Date:** 2026-08-02
**GitHub Issue:** [#51](https://github.com/ocx-sh/ocx-mirror/issues/51)
**Severity:** High

## Bug Report

### Observed Behavior

An `asset_type: archive` mirror publishes archive members with whatever mode
upstream shipped. PowerShell 7.6.0 ships `pwsh` at 0644 on Linux and macOS;
the published bundle carries 0644 and every consumer-side run is
`Permission denied (os error 13)`. `ocx-mirror package validate` exits 0.

### Expected Behavior

Every name in the metadata's `binaries` list is executable (0755) in the
published bundle — matching what `place_binary` already guarantees on the
`asset_type: binary` path.

### Reproduction Steps

1. Spec with `asset_type: archive`, metadata declaring `binaries: ["pwsh"]`,
   interface PATH = bare `${installPath}` (forces `bin_scan: off` — spec
   validation rejects scanning with no target dir, exit 65).
2. Upstream tarball carries `pwsh` at 0644.
3. `ocx-mirror package pipeline prepare` → `tar tvJf bundle.tar.xz` shows
   `-rw-r--r-- pwsh`.

### Environment

| Factor | Value |
|--------|-------|
| Platform | linux + macOS legs (unix exec bit) |
| ocx-mirror version | 0.5.1 (always broken — original implementation) |
| Registry | any |
| Configuration | `asset_type: archive`, `bin_scan: off`, declared `binaries` |

### Frequency

Always, for any archive whose members lack the exec bit.

## Root Cause Analysis

### Root Cause Statement

> Archive-sourced bundles keep upstream's mode bits because `extract()`'s
> `AssetType::Archive` arm delegates to `ocx_lib::Archive::extract_with_options`
> (tar preserves modes; zip defaults to umask without a unix-mode field) and no
> later step normalizes the declared interface binaries — while the
> `AssetType::Binary` arm chmods 0755 in `place_binary`. Present since the
> original implementation; asymmetry, not missing data: `resolve_binaries`
> passes the declared `binaries` claim through verbatim in Off/Auto-declared
> modes, so it is in scope at bundle time.

### Related Code

| File | Lines | Role |
|------|-------|------|
| `src/pipeline/package.rs` | 26-40, 58-76 | `extract()` arms; `place_binary` chmod (the asymmetry) |
| `src/pipeline/orchestrator.rs` | 662-687 | `prepare_task` bundle window (extract → bin_scan → libc → bundle) |
| `external/ocx/.../package/bin_scan.rs` | 393, 417 | Off/Auto-declared pass-through of the `binaries` claim |

### Pattern Check

- [x] Similar code searched — `place_binary` is the only chmod; no other placement path
- [x] Not a regression — original implementation
- [x] Other callers — both asset types route through `prepare_task`; fix lands there

## Regression Test Specification

| Test | File | Asserts |
|------|------|---------|
| `a_declared_binary_shipped_without_an_exec_bit_is_published_executable` | `src/pipeline/orchestrator.rs` | Bundle extracted from prepare: `pwsh` 0644→0755; `LICENSE.txt` stays 0644 (no blanket chmod) |
| `declared_binaries_are_made_executable_anywhere_in_the_tree` | `src/pipeline/package.rs` | Helper: nested match chmods, non-match untouched, absent declared name → Ok (written with fix — names a new fn) |

## Fix Approach

New `ensure_declared_binaries_executable(content_dir, binaries)` in
`src/pipeline/package.rs` (full-tree walk via ocx_lib `DirWalker` +
`scan_directory_files`; chmod 0755 on file-name match lacking `0o111`;
`#[cfg(unix)]` like `place_binary`). Called from `prepare_task` between
`reject_empty_scan` and `check_declared_libc` for all asset types and all
bin_scan modes. Full design: session plan + issue #51.

| File | Change |
|------|--------|
| `src/pipeline/package.rs` | helper + unit test |
| `src/pipeline/orchestrator.rs` | call site + regression test + doc-comment step list |
| `docs/reference/mirror-yml.md` | paragraph under `## metadata` |
| `.claude/rules/subsystem-mirror.md` | prepare-phase step list |

### Alternatives Considered

| Approach | Rejected Because |
|----------|-----------------|
| Chmod only interface PATH dirs | Repro's PATH is bare `${installPath}` → zero scan target dirs; fixes nothing |
| Error instead of chmod | Leaves spec author no lever — no spec knob marks an archive member executable |
| Warn/fail on declared-but-absent name | False warnings on Windows legs (`pwsh` vs `pwsh.exe`); ADR says declared-but-absent is legal |

### Risk Assessment

| Risk | Mitigation |
|------|------------|
| Data file named like a declared binary gets 0755 | Accepted — publisher declared the name a command; tar normalizes modes anyway |
| Resume path (bundle exists) never chmods | Same documented semantics as libc_lint resume; operator discards bundle |

## Verification Checklist

- [x] Regression test failed on pre-fix code (`left: 420, right: 493` captured)
- [x] Fix applied — regression test passes; 0o775 fixture pins the no-downgrade skip (mutation-checked red)
- [x] All existing tests pass (`task rust:verify` 594/594; full `task verify` at branch end)
- [x] Bundle round-trip asserted in test (extracted bundle `pwsh` 0755, `LICENSE.txt` 0644)
- [x] No scope creep — review round 1: 4 actionable fixed (windows-build imports, test gap, doc precedence, resume caveat), 2 deferred (verify-mode no self-heal; full-tree name collisions) — recorded in commit body

## Notes

Follow-up (not here): `bin_scan: verify` on bare-`${installPath}` specs —
needs ocx_lib change, upstream. Related: ocx-sh/ocx#268.
