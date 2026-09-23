# Measurement: Bazel vs nextest CI lane (ADR C11)

**Verdict: NO-GO.** CI tests stay on nextest. Bazel stays mandatory for the local/agent loop.

- Bar: `adr_bazel_crate_split.md` § C11, fixed by the ADR's landing commit
  [b2f974ea](https://github.com/ocx-sh/ocx-mirror/commit/b2f974eadf391d14468e8df47e33571b4a144c44)
  (committed 2026-09-22T21:36:38Z), which precedes the first measured run's `startedAt`
  (2026-09-23T10:04:26Z).
- Commit measured: [a57e57d7](https://github.com/ocx-sh/ocx-mirror/commit/a57e57d7445f39aa4b23b87f2ed7889c7e54f614)
  on the never-merged branch `measure/bazel-lane` = the phase-3 tree at
  [ffe35638](https://github.com/ocx-sh/ocx-mirror/commit/ffe35638cced3507f1a8d98d79fe1d215576c27e)
  plus `.github/workflows/bazel-lane-measure.yml`. Both lanes, same commit, same run.
- Run: [35846656688](https://github.com/ocx-sh/ocx-mirror/actions/runs/35846656688) — attempt 1 =
  the discarded warm-up (saved the warm caches), attempts 2–6 = the five measured runs
  (`gh run rerun`, strictly sequential). Metric: job `started_at`→`completed_at`.
- Lanes: nextest = today's `Smoke (Linux)`; Bazel = the same job with Build + Test replaced by
  `task bazel:test:unit JOBS=auto -- --config=ci` + `bazel build --config=ci //:ocx-mirror`
  (+ `setup-ocx`, which `ocx exec bazel` needs). Upload/publish steps dropped from both lanes
  alike. Cold = nothing restored (setup-rust's built-in cache and setup-ocx's store cache off
  in every leg). Warm = nextest: `Swatinem/rust-cache` keyed on the SHA; Bazel:
  `~/.cache/ocx-mirror/bazel-disk` + `bazel-repo` via `actions/cache` keyed on the SHA. No remote
  cache. Bootstrap/repin counts inside the Bazel lane.
- A first run on [3d8761c7](https://github.com/ocx-sh/ocx-mirror/commit/3d8761c7f1ce437d160ff11a01e8f57297e734c3)
  ([35844178305](https://github.com/ocx-sh/ocx-mirror/actions/runs/35844178305)) was abandoned
  before any rerun: setup-ocx restored its store cache in the cold Bazel leg. Its caches were
  deleted.

## Numbers (seconds)

| Attempt | nextest cold | nextest warm | Bazel cold | Bazel warm | Warm cache hit (nextest / Bazel) | Bazel JUnit / libtest |
|---|---|---|---|---|---|---|
| 1 (warm-up, discarded) | 1258 | 1347 | 1239 | 1086 | false / miss | 1570/1570 · 1570/1570 |
| 2 | 1065 | 730 | 1417 | 733 | true / true | 1570/1570 · 1570/1570 |
| 3 | 1311 | 631 | 1370 | 643 | true / true | 1570/1570 · 1570/1570 |
| 4 | 1312 | 720 | 1463 | 776 | true / true | 1570/1570 · 1570/1570 |
| 5 | 1101 | 723 | 1395 | 746 | true / true | 1570/1570 · 1570/1570 |
| 6 | 1278 | 602 | 1424 | 738 | true / true | 1570/1570 · 1570/1570 |
| **median (2–6)** | **1278** | **720** | **1417** | **738** | | |

Per-job URLs: `…/actions/runs/35846656688/job/<id>` — attempt 2: 107141596700, 107141596585,
107141596553, 107141596404; 3: 107149118946, 107149119183, 107149119341, 107149119363;
4: 107156249849, 107156250223, 107156250002, 107156250104; 5: 107164224391, 107164224352,
107164224299, 107164224570; 6: 107171698653, 107171698881, 107171699008, 107171698875
(order: nextest cold, nextest warm, Bazel cold, Bazel warm). Collector:
`.tmp/p3/collect.py` → `.tmp/p3/c11-35846656688.json` (local scratch).

## Criteria

| # | Criterion | Measured | Result |
|---|---|---|---|
| 1 | warm Bazel ≤ 0.75 × warm nextest **and** saving ≥ 120 s | 738 vs 540 (0.75 × 720); saving −18 s | **fail** |
| 2 | cold Bazel ≤ 1.15 × cold nextest | 1417 ≤ 1469.7 | pass |
| 3 | per-test JUnit count == libtest total on every Bazel run | 1570 == 1570 on all 10 | pass |
| 4 | zero failures, zero flakes over the 10 Bazel runs | 10/10 success, no `FLAKY` | pass |
| 5 | Bazel-built binary passes the C10 oracle | 202 / 0 / 2, baseline set (ledger § Oracle results, final) | pass |

## Where the time goes (median step seconds, attempts 2–6, steps ≥ 15 s)

| Step | nextest warm | Bazel warm | nextest cold | Bazel cold |
|---|---|---|---|---|
| Build + Test (nextest) / `bazel:test:unit` + binary (Bazel) | 134 + 245 | 72 | 329 + 290 | 768 |
| Build ocx (submodule pin), cargo | 249 | 509 | 506 | 502 |
| Clippy, cargo | 17 | 90 | 86 | 90 |
| Cache restore | 21 | 16 | — | — |

Warm Bazel removes ~300 s from the surface it replaces (379 s → 72 s). The job keeps three cargo
steps C11 does not replace — the ocx submodule build, clippy, the jsonschema check — and the
C11 cache definition gives only the nextest lane a cargo cache, so the Bazel lane pays those
steps cold (+260 s ocx build, +73 s clippy). Net: a wash. A lane that also carried a cargo cache
for the unreplaced steps would likely clear the bar; that is a new bar, so it needs a new ADR and
fresh runs (C11: "A NO-GO can be overturned only by a new ADR").

## NO-GO follow-through

- CI stays on nextest (`Smoke (Linux)` → `task rust:test:nextest -- --profile ci`); the
  `Bazel graph (Linux)` analysis job runs on every PR under either verdict.
- The per-test JUnit of the final local Bazel run (1570 cases, tree
  [4b1f40f3](https://github.com/ocx-sh/ocx-mirror/commit/4b1f40f3ddb0258a0e07543f73b3295ccb8a9f66))
  is kept at `.tmp/p3/bazel-junit-final.xml` for attachment to the mirror PR when it opens.

## Local (non-gating) numbers

- Cold `bazel build //...` on the dev host: 168.7 s, 1,509 actions, peak RSS of server + actions
  2.97 GB (per-process sampler; `/usr/bin/time -v` sees only the 14.6 MB client).
- Warm no-change `task bazel:cache:check`: `Executed 0 out of 15 tests`, `cachedLocally: 15`.
