# Bug Fix Plan: retry transient push failures — max_retries parsed and ignored

## Status

- **Plan:** bugfix_push_retry
- **Active phase:** 7 — Commit & Document
- **Step:** awaiting /finalize
- **Last update:** 2026-08-02 (after 5bd1648: fix(pipeline): retry transient push failures up to max_retries)

---

## Overview

**Status:** Approved
**Author:** Claude (planned with owner approval)
**Date:** 2026-08-02
**GitHub Issue:** [#50](https://github.com/ocx-sh/ocx-mirror/issues/50)
**Severity:** High

## Bug Report

### Observed Behavior

A transient GHCR blob-upload timeout (`ocx package push` exit 69) drops a
`(version, platform)` tile for the whole run despite the spec setting
`concurrency.max_retries: 3` — the field is parsed and never read. Observed on
ocx-contrib/mirror-amazon run 30751704164 (2/18 tiles dropped; re-run pushed
both cleanly).

### Expected Behavior

Transient push failures retried up to `max_retries` with bounded backoff; a
hung push bounded by a timeout (announce already has one); non-transient
failures fail immediately.

### Reproduction Steps

1. Spec with `concurrency.max_retries: 3`.
2. `ocx package push` subprocess exits 69 once (registry blip).
3. `pipeline push` records `PlatformFailure { reason: "push_error" }` after a
   single attempt; run goes red.

### Environment

| Factor | Value |
|--------|-------|
| Platform | any (CI runners) |
| ocx-mirror version | 0.5.1 (never implemented — `#[allow(dead_code)]` since origin) |
| Registry | GHCR (observed); any |
| Configuration | `concurrency.max_retries` set by most fleet specs |

### Frequency

Intermittent — any transient registry fault during phase-2 push.

## Root Cause Analysis

### Root Cause Statement

> `ConcurrencyConfig.max_retries`/`max_pushes` were declared for a
> "when concurrency control is implemented" future that never landed for push:
> `invoke_push` makes exactly one `.output().await`, has no timeout (announce
> has `tokio::time::timeout` + `kill_on_drop`), and stringifies the exit status
> without inspecting it — so transient (69) and permanent failures are
> indistinguishable and never retried.

Correction to the issue: `rate_limit_ms` is NOT dead — consumed at
`sync.rs:323` → `github_release.rs:78-79` as the source-API paging delay. It
stays and is not wired into push backoff (one number, two owners otherwise).

### Related Code

| File | Lines | Role |
|------|-------|------|
| `src/spec/concurrency_config.rs` | 7, 13-18 | dead `max_pushes` + unread `max_retries` |
| `src/command/package/pipeline/push.rs` | 864-898 | `invoke_push` single attempt, no timeout |
| `src/command/package/pipeline/push.rs` | 192-226 | phase-2 loop recording `push_error` |
| `src/command/package/pipeline/push.rs` | 1023, 1145-1149 | announce timeout pattern to mirror |

### Pattern Check

- [x] Similar code: `patch.rs::republish` (:324-333) same unretried push — deferred follow-up (helper signature chosen for 3-line adoption)
- [x] Not a regression — never implemented
- [x] Discord retry (discord.rs:128-295) webhook-specific, pattern only

## Regression Test Specification

Fixture `tests/fixtures/mirror-push-retry.yml` (mirror-minimal +
`concurrency: { max_retries: 1 }` — differs from default 3 to prove the field
is read). Stateful shell fake `fake_ocx_flaky_push(dir, failures, exit_code)`
with attempt-counter file.

| Test | File | Asserts |
|------|------|---------|
| `a_transient_push_failure_is_retried_and_the_tile_still_lands` | `push.rs` | fail-once-69 → run Ok, 2 attempts, tile published. **Red pre-fix** |
| `push_retries_stop_at_the_spec_max_retries` | `push.rs` | always-69 → exactly 2 attempts (bounded + spec-sourced), run Err. **Red pre-fix** |
| `a_non_transient_push_failure_is_not_retried` | `push.rs` | always-65 → exactly 1 attempt. Guard (passes pre-fix by construction) |
| `a_hung_push_is_killed_by_the_push_timeout` | `push.rs` | `sleep 10` fake, 200 ms timeout → Err, transient, elapsed < 10 s |
| `only_a_registry_fault_is_worth_retrying` | `push.rs` | transient table: 69/75 true; 1/64/65/70/74/77/78/None false |
| `a_spec_that_still_sets_max_pushes_keeps_parsing` | `spec.rs` | fleet-compat pin (no `deny_unknown_fields`) |

No acceptance pytest — forcing a 69 against a real registry needs a proxy;
cost exceeds coverage over the unit suite.

## Fix Approach

`push_once(binary, argv, timeout)` extracted from `invoke_push` body +
`kill_on_drop` + `tokio::time::timeout` (`PUSH_TIMEOUT = 900s`); retry loop in
`invoke_push` (signature unchanged) on transient failures up to
`spec.concurrency.max_retries` with saturating exponential backoff (base 1 s,
cap 30 s, no jitter — sequential push, no herd). Transient = exit 69, exit 75
(`ocx_lib::cli::ExitCode`, no magic numbers), our timeout. Drop `max_pushes`
+ struct-level `#[allow(dead_code)]` from `ConcurrencyConfig`. Full design:
session plan + issue #50.

| File | Change |
|------|--------|
| `src/command/package/pipeline/push.rs` | consts, `PushAttemptError`, `push_exit_is_transient`, `push_retry_backoff`, `push_once`, retry loop, tests |
| `src/spec/concurrency_config.rs` | remove `max_pushes`, drop `#[allow(dead_code)]`, doc comments |
| `src/spec.rs` | delete `:1555` assertion; add fleet-compat test |
| `tests/fixtures/mirror-push-retry.yml` | new fixture |
| `docs/reference/mirror-yml.md` | `:22` concurrency table cell |

### Alternatives Considered

| Approach | Rejected Because |
|----------|-----------------|
| Wire `rate_limit_ms` into push backoff (issue's ask) | Field is live for source paging — two owners for one number |
| Retry every failure | Permanent failures (65/77/78) waste 3 × 15 min per tile and mask real defects |
| Keep `max_pushes` documented-unused | Silently ignored config is the trap this issue names; push is architecturally sequential |

### Risk Assessment

| Risk | Mitigation |
|------|------------|
| Retry after partial/completed upload | Push idempotent: blob HEAD-check, per-platform index merge; completed → `skipped_existing`, already handled |
| Worst case job time | 15 min × 4 attempts = 1 h/tile, under 6 h GHA cap; run-summary still written |
| Fleet specs still setting `max_pushes` | No `deny_unknown_fields` on `ConcurrencyConfig` — key ignored; pinned by test |

## Verification Checklist

- [x] T1/T2 failed on pre-fix code (captured: 1 attempt, run Err)
- [x] Fix applied — T1–T6 pass + `max_retries_zero_is_a_single_attempt`; T4 kill_on_drop mutation-checked red/green; fixture at `max_retries: 2` rules out hardcoded 1 and 3
- [x] All existing tests pass (`task verify` exit 0, 601 unit + 42 acceptance)
- [x] No scope creep — `invoke_push` signature + error strings byte-identical; review round 1: 5 actionable fixed, deferred noted in commit body (permanent 403 retried until ocx#266; fleet-wide default-3 activation)

## Notes

Follow-ups: `patch.rs::republish` retry adoption; ocx-sh/ocx#266 narrows
transient set to 75; ocx-sh/ocx#267 blob-chunk retry (complementary).
