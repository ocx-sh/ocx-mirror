# Research: operability and cost of a registry mirror run

**Axis:** operability & cost
**Date:** 2026-08-13
**For:** `adr_registry_mirror_sync.md` (`ocx-mirror registry sync`)

---

## 1. Sizing — measured

| Input | Value | Method |
|---|---|---|
| Packages | **121** | Live fetch of `https://index.ocx.sh/c/index.json`, 2026-08-13, `format_version: 1`. **Corrected**: an earlier pass in this same session recorded 227 from the same URL on the same day. Re-fetched twice, cache-busted, by two independent readers: 121 both times. Coherent with the local `/home/mherwig/dev/index` checkout at 117 — which that pass called "stale vs live 227", when 117 vs 121 in fact says the checkout is current and 227 was the outlier. Every ceiling below derives from this number and roughly halves against the original |
| Release-builds per package | **~13.1** | Local checkout `/home/mherwig/dev/index` (117 packages, coherent with the live 121): 1532 `p/*/*/o/sha256/*.json` ÷ 117 = 13.09. Hand-checked on 10 packages (bazel 53, cmake 15, ocx/cli 17, regsync 10, buildifier/bazelisk/buildozer/unused-deps 7 each, python-build-standalone 6, ocx/mirror 1) → mean 13.0. Not bazel-skewed |
| Platforms per release | **~6** | 3 sampled dispatch objects: bazel 5, cmake 6 (2 platforms alias 1 digest), python-build-standalone 7 (2 alias 1 digest) |
| Bytes per platform blob | **~22 MB blended** *(weakest input)* | Real GitHub Releases API sizes: ripgrep 15.2.0 mean 1.87 MB/platform; bazel 9.0.2 mean 61.0 MB/platform; python-build-standalone `install_only` ~33 MB. Catalog composition blended 55% small (5 MB) / 30% medium (30 MB) / 15% large (70 MB) = 22.25 MB. Plausible range 15–35 MB |

**Arithmetic (full public catalog, cold, empty destination):**

- Release-builds: 121 × 13.1 ≈ **1,585**
- Platform blobs: 1,585 × 6 ≈ **9,510**
- Bytes: 9,510 × 22 MB ≈ **210 GB** (range 48–285 GB → **low hundreds of GB**)
- Requests: 2 (catalog + config) + 121 (roots) + 1,585 (source image-index GET)
  + 9,510×3 (source manifest+config+layer) + 9,510×3 (destination PUTs)
  + 1,585 (destination index PUT) ≈ **60,000** → order **10⁴–10⁵**

At 50 Mbps that ceiling is ~9 h; at 1 Gbps, ~28 min. **This is the stress-test
ceiling, not the typical case** — the glob filter means a real corporate first
run is tens of packages, not 121.

---

## 2. Resumability — no journal needed

`adr_servable_index_snapshot.md` Known tension #3 records resumability as
unsolved for `ocx index sync`. That tension is about refetching small JSON
documents (cheap). **Here "refetch everything" is the 210 GB number**, so the
question is real.

| Tool | State persisted | Mechanism |
|---|---|---|
| apt-mirror | None historically; partials silently left incomplete ([#98](https://github.com/apt-mirror/apt-mirror/issues/98)). 2026-03 release moved to near-atomic `dists` replace via `move` | Atomic rename of final state |
| debmirror | Explicit state cache, `--state-cache-days` expiry | Journal with expiry |
| **reprepro** | **None, by design** — *"since the upstream mirror remains consistent, reprepro will always download a consistent set of files"* + atomic release ⇒ *"no period where the package index doesn't match the filesystem"* ([OpenDev](https://docs.opendev.org/opendev/system-config/latest/reprepro.html)) | **Indexes are derived, not journaled** — the same philosophy as `ocx index regenerate` |
| rsync | `--partial` / `--append-verify` | Sub-file resume — unnecessary here; blobs are whole-object digest-addressed |
| zot sync | `maxRetries`/`retryDelay` only | Retry the operation, not resume mid-copy |
| regsync | None documented | Pure re-run; relies on registry-side digest checks |
| Harbor | Execution/task records, auto-retry | Journal at task grain, not byte offset |

**Two tiers of "cheap", quantified:**

1. **Destination HEAD per blob** — 9,510 HEADs to resume a full catalog: **~16%
   of a cold run's requests, ~0% of its bytes.** Scales with total blob count,
   not with what changed.
2. **Catalog-digest short-circuit** (§3) — the source catalog already maps
   package → sha256(root). Comparing against roots already on disk (zero
   network) identifies changed packages before any blob is touched. A resume
   after interruption then needs tier-1 HEADs for **only the one package that
   was mid-flight**.

**Design consequence — resumability falls out of a rule we already need for
correctness.** Write the package's index root **only after every referenced blob
is confirmed present at the destination** (the atomic-visibility ordering this
design already commits to). Then **the output directory is the checkpoint**: on
interrupt, at most one package has blobs copied and no root written. Detect by
root-absent, redo that package with HEAD-skip — worst case ~150 requests for one
package, not 60,000.

**v1 answer: no journal.** Write-root-last plus destination-HEAD-skip is
sufficient, and costs nothing beyond ordering discipline already required.

---

## 3. Incremental runs — near-free

The source catalog is `packages[package_id] = sha256(root_raw)` wrapped in
`{"format_version", "packages"}` (`/home/mherwig/dev/index/bot/src/indexbot/core/render.py:293-309`).

**A no-op sync of N packages costs exactly 1 HTTP request, independent of N.**
Fetch `c/index.json`, compare each filtered package's digest against the locally
written root (a local read), find zero deltas, exit. 10 packages or 121 — same
one request.

A run with K changed packages costs 1 + K root refetches + only the new dispatch
objects and blobs those K roots reference. Real deltas are small: a version bump
adds ~1 release-build (~6 blobs) per changed package, not its whole history.

---

## 4. Rate limits

| Registry | Documented limit | Reality |
|---|---|---|
| ghcr.io | **None published** — GitHub states container storage/bandwidth is currently free, no request quota on the [container-registry docs](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry) | 429s observed under burst/shared-IP load ([NVIDIA/cuda-quantum#3979](https://github.com/NVIDIA/cuda-quantum/issues/3979), [community#49671](https://github.com/orgs/community/discussions/49671)) |
| Docker Hub | 100 pulls/6 h anonymous per IPv4 or IPv6-/64; 200/6 h authenticated free; unlimited paid ([docs](https://docs.docker.com/docker-hub/usage/pulls/)) | 429 on exceed |
| regsync's answer | `ratelimit: {min, retry}` — **proactive**: reads the remaining-pull header *before* starting a step and pauses when it drops below `min` (default `min: 20, retry: 15m`) | **Needs a rate-limit header to read**, which GHCR does not reliably publish — so the proactive model has no signal against our likely primary source |

Note the shape difference precisely: regsync's `ratelimit` is a *pre-flight quota
check*, not a 429 handler. Those are different mechanisms, and only the reactive
one works without a published header. Named precedents worth citing for the
destination-skip behaviour recommended in §2: regsync's `once --missing` (copy
only missing tags) and its `fastCopy` / `forceRecursive` pair (skip an existing
target versus re-verify it). regsync persists **no** resume state — confirmed on
two independent documentation fetches.

**Implication:** GHCR — today's primary source via `ocx-contrib/*` — has no quota
to plan against but does 429 under load. Proactive quota tracking has no signal
to read. The correct v1 behaviour is **reactive**: honour 429 / `Retry-After`
with backoff, reusing the shape already in `push_with_retry`
(1s doubling to 30s cap, ±10% jitter). Revisit proactive throttling only if a
Docker Hub source is added.

---

## 5. Observability and failure semantics

| Tool | Partial failure | Reporting |
|---|---|---|
| skopeo sync | Abort on first failure; `--keep-going` logs and continues but still exits non-zero | Exit code is the only signal |
| regsync | `check` mode — *"reports if any images need to be synchronized, but does not copy any content"* | Explicit dry-run report |
| Harbor | Auto-retries a failed task; always produces a visible execution record, even for a no-op | Structured record |

Harbor's model read from its Go source rather than its docs: an `Execution`
carries live `Total` / `Failed` / `Succeed` / `InProgress` / `Stopped` counters
and a `Task` is per-artifact — confirming continue-on-error is Harbor's actual
behaviour, not abort-on-first. **That counter set is the right shape for this
command's summary line**: "N total, M copied, K skipped, J failed". Harbor also
carries a *single active replication* toggle preventing overlapping runs of one
rule — worth stealing, and it rhymes with the cross-mirror concurrency invariant
this repo already enforces on generated workflows (`subsystem-mirror.md` § R1).

**Reuse the existing taxonomy, do not invent one.** `MirrorError` / `ExitCode`
is already exhaustively mapped (`src/error.rs:8,75-93`). And
`src/pipeline/target_registry.rs` already solves the exact problem the
destination skip-check needs: `RepositoryNotFound` is treated as legitimately
empty (`:238-244`), while a `ManifestNotFound` for a tag the source just listed
is refused as an error rather than read as absent (`:420-428` — the test is
literally named *"a child manifest the index just listed must not read as
absent"*). Reuse that fail-safe pattern; do not reinvent it.

**v1 behaviour:**

- **Continue-on-error, fail-at-end** (skopeo `--keep-going` semantics) as the
  default — one broken package must not abort 226 healthy ones. Non-zero exit
  iff anything failed.
- **Never silent on a no-op** — print "N packages checked, 0 changed". Silence is
  indistinguishable from "did not run" in a CI log.
- **`--dry-run` reports counts *and* estimated bytes.** Extend the existing
  `Check` command pattern (`src/command/package/check.rs`). Bytes matter: it is
  the number an operator on a metered link needs before committing to a run.

---

## 6. Bandwidth controls

| Tool | Knob | Default |
|---|---|---|
| Harbor | Chunked copy (`REPLICATION_CHUNK_SIZE`, 10 MB) — **off by default**; bandwidth cap in KB/s (`-1` unlimited) | Chunking disabled |
| zot | `maxRetries`, `retryDelay` | No bandwidth cap |
| regsync | `ratelimit: {min, retry}` (quota, not bandwidth) | `min: 20, retry: 15m` |

**Opinionated minimum: bounded concurrency, nothing else.** A semaphore on
in-flight blob copies is what keeps a run from either hammering GHCR into 429s or
saturating a corporate link, and the convention already exists
(`buffer_unordered(8)`, `INDEX_REFRESH_CONCURRENCY`). A KB/s cap and chunked
upload are premature: the largest asset sampled in the catalog is bazel's
`dist.zip` at 221 MB, nowhere near Harbor's multi-GB chunking case.

---

## Opinionated minimum — v1 operability surface

**Ship:**

1. Catalog-digest short-circuit — no-op cost O(1), changed-package cost O(K).
2. Write-root-last ordering, output tree as checkpoint — resumability, zero journal.
3. Destination-HEAD content-addressed skip, reusing `target_registry.rs`'s
   authoritative-versus-ambiguous-404 pattern.
4. Extend `MirrorError` / `ExitCode` rather than inventing exit semantics.
5. Continue-on-error / fail-at-end; non-silent no-op; `--dry-run` reporting
   counts **and** estimated bytes.
6. Reactive 429 / `Retry-After` backoff reusing `push_with_retry`'s shape.
7. Bounded concurrency semaphore on blob copies.

**Defer, each with the number that un-defers it:**

| Deferred | Trigger |
|---|---|
| Persisted resume journal | A filtered sync exceeds **~50,000 blobs** in one invocation (≈5× the corrected full-catalog estimate of 9,510) **and** mid-run failures become more than occasional |
| KB/s bandwidth cap | A real run is measured saturating **>80% of a shared link for >10 min** |
| Chunked blob upload | A catalog blob exceeds **~500 MB–1 GB**, or repeated mid-transfer failures appear on today's sub-250 MB blobs |
| Proactive quota self-throttle | A **non-GHCR source** (e.g. Docker Hub) is added — GHCR does not publish the header this needs |
| `_catalog`-API enumeration | **Never, structurally** — static-catalog enumeration is this design's stated advantage over every surveyed competitor |
