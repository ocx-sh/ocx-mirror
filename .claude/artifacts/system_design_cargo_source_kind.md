# System Design: `source.type: cargo` — native per-platform crate builds in the mirror pipeline

**Companion to:** [`adr_cargo_source_kind.md`](./adr_cargo_source_kind.md)
**Owner:** Architect → `/hex-plan` → Builder
**Status:** Proposed
**Date:** 2026-09-17 (revised 2026-11-08)

> **Revision 2026-11-08 (still Proposed, never implemented).** Rewritten together with the ADR's
> two 2026-11-08 passes.
>
> **Pass 1:**
> - Paths point at the post-split crates, and `plan.json` is v4 → v5.
> - The `.crate` is a planned asset that the mirror fetches and verifies, then builds with
>   `cargo install --path` (no `--index`).
> - `prepare` provisions `rustup` and writes the hand-off layout.
> - Non-GitHub renders are first-class.
>
> **Pass 2 (Round 1 review):**
> - Build work is marked by `versions[].build`; the `prepare[]` units are withdrawn.
> - The `MirrorTask` seam is `build: Option<CargoBuild>`.
> - The read credential reaches `cargo fetch` only, and the build runs `--offline`.
> - Package-root `.cargo/` is skipped, and an ancestor cargo project is refused.
> - Fetch and unpack are bounded and type-checked.
> - The installed bins are checked against the plan, and a `request_sha256` binds `build.json`.
> - Annotations use uniform-or-`mixed` semantics.
> - Build kinds never resume.
> - `containers[].toolchain` is added to the plan.
> - The single-host isolation requirement is stated, and the cron reference keeps going after a
>   failed unit.

This spec defines the v1 component contracts at C4 levels 1–3, plus the code level where the ADR
names a seam. Contract IDs C1–C10 and S1–S22 are the ADR's; this spec says where each lives, how the
jobs connect, and how each rule is tested (§ 6). It does not assign work packages; that is
`/hex-plan`'s job. Paths and line numbers are against ocx-mirror @ `7524f91`.

---

## L1 — Context

```
                 sparse index (source.index)            dl host (config.json "dl" template)
                 config.json, <prefix>/<crate>           <crate>-<vers>.crate
                     ▲ plan: versions, cksum,               ▲ plan + prepare: cargo::fetch_crate
                     │       features (no redirects)        │ (≤ 50 MiB, host-gated creds, re-validated redirects)
 operator ─edits─► mirror.yml ─► ocx-mirror ───────────────┴──────────────────► target OCI registry
 (trusted)         type: cargo   plan → prepare × (V×P) → test → push           <repo>:<V>_<TS>
                   cargo: {…}      any CI (GitHub, GitLab, Jenkins) or cron     + cascade
                                        │                                       + sh.ocx.mirror.* (value | mixed)
                                        │ build legs: rustup → cargo fetch (read cred)
                                        │             → cargo install --offline (no cred)
                                        ▼ deps via the operator's $CARGO_HOME (replace-with, proxy, CA)
                       dependency registry: crates.io or a corporate remote (Artifactory, Nexus)

 ocx clients ◄── ocx package install ── target registry   ("built by ocx-mirror", never "mirrored")
```

| Actor / system | Trust (per `security-threat-model.md`) | What reaches us |
|---|---|---|
| Operator, `mirror.yml`, CI runner or cron host, `$CARGO_HOME`, rustup install | trusted execution environment | config and environment, never modelled |
| Sparse index + `dl` host, and any redirect target | **in-scope attacker** (compromised or MITM'd index) | `config.json`, index lines, `.crate` bytes, `Location` headers |
| Crate author and transitive dependencies | **in-scope attacker** | `build.rs`, proc-macros, and the built binary, which execute on build and test legs; package-local `.cargo/` and `rust-toolchain*` |
| Dependency registry / corporate remote | trusted config; foreign bytes | dependency `.crate`s, which `cargo fetch` checks against the packaged `Cargo.lock` |
| Rust dist server (`RUSTUP_DIST_SERVER`) | operator-chosen trust root | toolchain components, sha256-checked against the channel manifest over TLS |
| Target registry | trusted output | read at plan (tags) and at push (existing annotations for backfill); written at push |

---

## L2 — Containers (jobs)

Each container is one `ocx-mirror` invocation. A renderer (GitHub, GitLab, Jenkins, a script) only
schedules them; the edges below are the whole ordering contract.

```
┌ discover ──────────────────┐  plan.json ─────────────────────────────┐ (artifact store)
│ pipeline plan               │  plan_sha256 ───────────────┐           │ (forge-trusted channel:
│ class: source-read          │                              │           │  a job output, a literal in
│ no upstream code runs        │                              │           │  the generated child config,
└──────────────┬──────────────┘                              │           │  or the push user's shell)
               │ versions[]: without build → fetch leg       │           ▼
               │             with build    → build leg × p   │  ┌ prepare: build leg (V, p) ─────────┐
     ┌─────────┴────────────────────────┐                     │  │ runs on legs[p].runner             │
     ▼                                  ▼                     │  │ rustup → rustc -V                  │
┌ prepare: fetch leg (V) ───┐                                 │  │ → cargo fetch   [source-read here] │
│ any host                   │                                 │  │ → cargo install --offline [none]   │
│ class: ≤ source-read       │                                 │  │ UPSTREAM CODE RUNS (build.rs)      │
└────────────┬──────────────┘                                 │  └───────────────┬──────────────────┘
             └─────────────── bundles/ ◄───────────────────────────────────────────┘
                bundle-{V}-{slug}.tar.xz · -metadata.json · -build.json (build kinds)
                               │                              │
                               ▼                              │
                ┌ test (P, C) ─────────────────┐              │
                │ legs[P].runner, containers[] │              │
                │ (+ containers[].toolchain)   │              │
                │ every version's JUnit, then  │              │
                │ fail if any failed           │              │
                │ UPSTREAM CODE RUNS (binary)  │              │
                └──────────────┬───────────────┘              │
                               ▼ junit/                       ▼
                ┌ push ───────────────────────────────────────────────┐
                │ pipeline push --plan plan.json --plan-sha256 HEX      │
                │ class: registry-write (the ONLY holder, S21)          │
                │ planned-only · completeness · build records · mixed   │
                └──────────────┬──────────────────────────────────────┘
                               ▼ run-summary.json
                ┌ notify (webhook secret) ┐
```

| Job | One command (C3 rules) | Upstream code? | Class (S20) | Reads | Writes |
|---|---|---|---|---|---|
| discover | `pipeline plan --format json > plan.json` | no (the scan parses in-process) | `source-read` | spec, index, `.crate`, target tags | `plan.json` (+ `locks/` for pypi) |
| prepare, fetch leg | `pipeline prepare --plan plan.json --version V --bundles-dir bundles` | no | ≤ `source-read` | `plan.json`, assets | `bundles/` |
| prepare, build leg | the same, plus `--platform p` | **yes** | `source-read` reaches `cargo fetch` only | `plan.json`, `.crate`, deps | `bundles/` incl. `-build.json` |
| test (P, C) | rendered, per `plan-json.md` § Rendering (`pipeline test` is § Deferred to owner) | **yes** | ≤ `source-read` | `bundles/` | `junit/` |
| push | `pipeline push --plan plan.json [--plan-sha256 HEX] --bundles-dir … --junit-dir … --write-summary …` | no | `registry-write` | everything above, plus existing index annotations | registry, `run-summary.json` |
| notify | `pipeline notify` (unchanged) | no | webhook secret | `run-summary.json` | Discord |

**Trust boundaries.** Upstream code runs in exactly two job types, build-leg prepare and test, and
both hold at most `source-read`. Push consumes their outputs through a store any of them can write.
Three controls cover that:
- **The build record.** It binds bundle, `build.json` and plan, which catches corruption, staleness
  and omission.
- **The planned-only publish rule.** An unplanned bundle is never pushed.
- **`--plan-sha256`.** It travels outside the store, so a rewritten plan cannot change what push
  publishes.

A sibling overwriting a planned bundle is the accepted residual (ADR § Security). On one machine,
every boundary in this diagram exists only if the operator builds it (S21, § 5.2).

---

## L3 — Components

| Component | Path | Status | Responsibility | Contract |
|---|---|---|---|---|
| `Source::Cargo` | `crates/ocx_mirror_spec/src/source.rs` | changed | new `cargo` arm `CargoSource {crate, index, url_rewrite}`, `deny_unknown_fields` | C1 |
| `CargoConfig` | `crates/ocx_mirror_spec/src/cargo_config.rs` | **new** | the `cargo:` block, `deny_unknown_fields` | C1 |
| validation | `crates/ocx_mirror_spec/src/validate.rs` | changed | C1 rules; the `sh.ocx.mirror.` reservation (every kind); non-empty tests at load (cargo) | C1 |
| `rust_target_triple` | `crates/ocx_mirror_spec/src/platform_keys.rs` (beside `platform_key_slug`, `:118`) | **new fn** | total over the C8 keys, `None` otherwise | C8 |
| `ocx_mirror_source::cargo` | `crates/ocx_mirror_source/src/cargo.rs` | **new** | discovery (fail-closed lines); `fetch_crate` (one function for plan and prepare); `scan_crate`/`unpack_crate` (one bounded reader); `cargo_toml` manifest completion; feature closure; grammar newtypes; local `CargoSourceError` (E1) | C2 |
| SSRF-floored client | `crates/ocx_mirror_http/src/lib.rs` | moved | the guarded builder from `crates/ocx_mirror_pipeline/src/registry_sync/catalog.rs:176-183`, plus a per-hop re-validating redirect policy for the `.crate` | C2 |
| `CargoError`, `BuildEnvironment` | `crates/ocx_mirror_error/src/{lib.rs,cargo.rs}` | **new** | 65 / 78; `classify_error` maps `CargoSourceError` → 65/69 | C7 |
| `MirrorTask.build` | `crates/ocx_mirror_pipeline/src/mirror_task.rs:23-52` | changed | `build: Option<CargoBuild>`, `None` at every existing construction site | S2 |
| `cargo_build` | `crates/ocx_mirror_pipeline/src/cargo_build.rs` | **new** | rustup, rustc -V, fetch, offline install, child envs, scrub, static CRT, deployment target, log relay, bins check, receipts, `request_digest` | C5 |
| window | `crates/ocx_mirror_pipeline/src/orchestrator.rs` (`prepare_version` `:340`, `prepare_task` `:747`) | changed | build kinds: clean previous outputs, no resume, fetch/verify/unpack, build, metadata synthesis instead of the `:766-768` bail, `build.json` | C4, C5 |
| authoring metadata | `crates/ocx_mirror_pipeline/src/package.rs` | changed | synthesized sidecar for build kinds without `metadata:` | C5 step 7 |
| plan document | `src/command/package/pipeline/plan.rs` | changed | `PLAN_SCHEMA_VERSION = 5` (`:67`); `versions[].build`, `versions[].cargo`; schema `$id` v5 | C3 |
| cargo entry | `src/command/package/pipeline/plan/cargo.rs` | **new** | `cargo_entry()`: discovery + fetch + scan + refusals → `PlanCargoEntry` + assets + `build`. One function, three callers: `plan`, plan-less `prepare`, `check` | C2, C3 |
| legs | `src/command/package/pipeline/plan/legs.rs` (`PlanContainer`, `:39-53`) | changed | `toolchain` on the `_native_` container, derived through the same helper `matrix.rs:187` uses; parity test extended | C3 |
| drift | `src/command/package/pipeline/plan/drift.rs` | changed | skips cargo (the env precedent, `plan.rs:356-360`) | S9 |
| `prepare` | `src/command/package/pipeline/prepare.rs` | changed | `--platform` (archive + build; env → 64), `--bundles-dir` (all kinds), `requires` and path checks, the work-dir check, cargo task construction, the plan-less cargo path | C4 |
| `push` | `src/command/package/pipeline/push.rs`, `push/bundles.rs` | changed | `--plan`, a missing dir = zero bundles, the planned-only rule, `--plan-sha256`, build records, the `mixed` overlay, reasons | C6 |
| `check` / `sync` / `patch` | `src/command/package/check.rs`, `sync.rs`, `pipeline/patch.rs` | changed | `check` calls `cargo_entry`; `sync` and `patch` refuse cargo (64) | S9 |
| renderer | `src/command/package/pipeline/generate/ci.rs`, `ci/matrix.rs`, `templates/workflow.yml` (+ `templates/prepare_cargo.yml`) | changed | prerequisite fixes for every kind (`!cancelled()`, test loop), then the cargo branch only | C9 |
| docs | `docs/reference/{mirror-yml,plan-json,cli}.md`, `docs/corporate-cargo.md` (**new**), `docs/getting-started.md` | changed | C10 | C10 |

Every edge above is allowed by `crates/crate_map.toml`: spec→source, source→http, pipeline→source/spec/http,
root→all. No `ocx_*` row is added. New direct dependencies:
- `tar` and `flate2`, copied exactly from `external/ocx/Cargo.toml:213-214`;
- `semver` and `cargo_toml`, mirror-owned (ADR violation 10, § Existing crates considered).

### Data flow (one cargo version V, platforms P1…Pn)

1. **`plan`.** `cargo_entry()` reads `config.json` and the index file (no redirects), renders `dl`,
   fetches and scans the `.crate`, closes features, and computes the expected bins. It emits
   `versions[V]` with n `assets` (each the same `.crate` URL and `digest`), plus
   `build: {timeout_seconds}` and `cargo`.
2. **The renderer** projects `(V, p)` for `p ∈ versions[V].platforms`, and schedules each pair on
   `legs[p].runner`.
3. **`prepare --plan plan.json --version V --platform Pi --bundles-dir bundles`.** The checks
   (`requires`, path safety, `--platform`) run first. The filter leaves one `MirrorTask` with
   `build: Some(..)`. The task then:
   1. cleans the previous outputs;
   2. runs `cargo::fetch_crate`, then `verify` (digest required), then `cargo::unpack_crate` into
      `source/`;
   3. runs the work-dir check;
   4. builds via `cargo_build` (rustup, rustc, fetch, offline install, bins check, receipts);
   5. runs the unchanged window.

   It writes `bundle-{V}-{slug_i}.{tar.xz,-metadata.json,-build.json}` into `bundles/`.
4. **`test (Pi, C)`.** Unchanged, apart from the prerequisite loop fix.
5. **`push --plan`.** Checks run first: sha, `requires`, path safety, and cargo re-validation. Then
   push:
   1. treats a missing bundles dir as empty, and ignores unplanned bundles;
   2. records `missing_bundle` for every planned `(V, Pi)` with no bundle;
   3. checks every build record;
   4. computes V's overlay over the resulting index (value or `mixed`);
   5. pushes greens oldest-first;
   6. cascades V only when no `(V, Pi)` is red or missing.

### Prepare-window mapping (fetch kinds vs build kinds)

| Window step | Archive kinds (today) | Build kinds (cargo v1) |
|---|---|---|
| Task source | `versions[].assets[]` per platform | the same: the `.crate` is that platform's asset |
| Metadata config | `metadata:` required (`orchestrator.rs:766-768` bails) | optional: synthesized when absent (C5 step 7) |
| Resume | bundle + sidecar + byte-equal `authoring-metadata.json` + archive digest (`:777-812`) | **none** (S22): previous outputs are deleted, and every invocation rebuilds |
| Fetch | `download` (`download.rs`, uncapped, per-URL auth) | `cargo::fetch_crate`: floored client, 50 MiB cap, host-gated auth, re-validated redirects |
| Verify | `verify` per `DigestPolicy` | the same `verify`, with the digest **always required** |
| Extract | generic archive extractor → `content/` | `cargo::unpack_crate` → `source/`: 1 GiB / 100 000 entries, prefix, regular files and dirs only, package-root `.cargo/` and `rust-toolchain*` skipped |
| Work-dir check | — | ancestor `Cargo.toml` / `.cargo/config{,.toml}` → 78 |
| Build | — | `cargo_build` under the CPU permit |
| Bins check | — | `content/bin/` == plan `bins` |
| Receipts | — | delete `content/.crates.toml` and `content/.crates2.json` |
| chmod declared | before create, keyed on `binaries` (`:893-897`) | skipped: `bin_scan` is forced to `verify`, and cargo sets the mode bits |
| Authoring sidecar → `ocx package create` | `:899`, `:902-920` | the same; `--bin-scan verify` |
| `reject_empty_scan` → sidecar rename | `:925-927` | the same |
| Drop trees | `content/` (`:937`) | `content/`, `source/`, `target/` |
| Tier 2 post-process seam | between extract and chmod (follow-up ADR) | between receipts and chmod (follow-up ADR) |

---

## Code level (seams only)

Types and signatures are the contract. Bodies are `/hex-plan`'s.

```rust
// crates/ocx_mirror_pipeline/src/mirror_task.rs
pub struct MirrorTask { /* existing fields unchanged */ pub build: Option<CargoBuild> }

/// Tier 3 turns `Option<CargoBuild>` into an enum in its own ADR; one variant today is YAGNI.
pub struct CargoBuild {
    pub krate: CrateName,                 // ocx_mirror_source::cargo newtypes: constructed only
    pub version: semver::Version,         // through the C1/C2 grammars, so no argv token is
    pub channel: RustChannel,             // ever an unvalidated string
    pub features: Vec<CargoFeature>,      // sorted, deduped
    pub no_default_features: bool,
    pub bins: Vec<BinName>,               // the expected set from the plan, sorted
    pub triple: &'static str,             // rust_target_triple(platform key)
    pub timeout: std::time::Duration,     // versions[].build.timeout_seconds
    pub scrub_extra: Vec<String>,         // the spec's sign: env://NAME names
}

/// Returned by cargo_build::run; the rustc line goes into build.json.
pub struct BuildFacts { pub rustc: String }

/// sha256 over the canonical request: compact JSON, field order
/// crate, version, digest, channel, features, no_default_features, bins, target; sorted arrays.
/// Called by prepare_version (writes build.json) and by push (recomputes from the plan).
pub fn request_digest(build: &CargoBuild, asset_digest: &str) -> String;
```

```rust
// src/command/package/pipeline/plan.rs
pub const PLAN_SCHEMA_VERSION: u32 = 5;

pub struct PlanVersionEntry {
    /* existing fields unchanged */
    #[serde(default, skip_serializing_if = "Option::is_none")] pub build: Option<PlanBuild>,
    #[serde(default, skip_serializing_if = "Option::is_none")] pub cargo: Option<PlanCargoEntry>,
}
pub struct PlanBuild {
    pub timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")] pub requires: Vec<String>,
}
pub struct PlanCargoEntry {
    #[serde(rename = "crate")] pub krate: String,
    pub version: String, pub index: url::Url, pub channel: String,
    pub features: Vec<String>, pub no_default_features: bool, pub bins: Vec<String>,
}

// src/command/package/pipeline/plan/legs.rs — PlanContainer gains:
//   #[serde(default, skip_serializing_if = "Option::is_none")] pub toolchain: Option<PlanToolchain>,
pub struct PlanToolchain { pub project: String, pub groups: Vec<String> }   // groups effective: ["default"] when none
```

- `requires` is `Vec<String>`, not an enum. The mirror emits no value in v5. The field exists so a
  *future* value reaches an *old* reader as an unknown it must refuse. The mirror's own readers
  (`prepare`, `push`) apply the same refusal.
- `build` is set on every non-drift entry of a cargo spec and on nothing else. That is a pure
  function of spec and entry kind, tested as a table.

```rust
// crates/ocx_mirror_spec/src/platform_keys.rs
pub fn rust_target_triple(key: &str) -> Option<&'static str>;   // C8 table; None → SpecInvalid at validate
```

**`build.json`** follows the ADR C5 shape exactly: `{version, platform, sha256, request_sha256,
rustc}`. `prepare_version` writes it last, once the bundle digest is known (`orchestrator.rs:402`).
`--bundles-dir` copies it as `bundle-{V}-{slug}-build.json`. Push parses it with
`deny_unknown_fields`, so a field added later needs prepare and push from the same release. The
renderers ship them together, and a skewed pair reads as `build_record_missing`, which is loud.

**Order inside one build task.**
1. Delete the previous outputs.
2. Fetch, verify and unpack.
3. Run the work-dir check.
4. Run rustup, rustc, fetch and install.
5. Run the bins check, then delete the receipts.
6. Run the window: sidecar, create, scan, rename.
7. Write `build.json`.

A failure at any step leaves no `build.json` for this run, so push reads `build_record_missing` or
`missing_bundle`, never a stale record.

---

## § 1 Spec additions

C1 is the authority for every field rule. This section gives only the shape:

```rust
// ocx_mirror_spec::source — #[serde(tag = "type")] arm "cargo", deny_unknown_fields
pub struct CargoSource {
    #[serde(rename = "crate", default)] pub krate: Option<String>,  // default: spec.name
    #[serde(default)] pub index: Option<url::Url>,                   // default: https://index.crates.io
    #[serde(default)] pub url_rewrite: Option<UrlRewrite>,           // existing shape; .crate host only
}
// ocx_mirror_spec::cargo_config — deny_unknown_fields; MirrorSpec gains `cargo: Option<CargoConfig>`
pub struct CargoConfig {
    #[serde(default)] pub channel: Option<String>,          // default "stable"; validated via RustChannel
    #[serde(default)] pub features: Vec<String>,            // CargoFeature
    #[serde(default)] pub no_default_features: bool,
    #[serde(default)] pub bins: Option<Vec<String>>,        // BinName
    #[serde(default)] pub timeout_seconds: Option<u64>,     // default 3600, 60..=21600
}
```

- **Owner shape preference.** Every v1 field is a literal. A field that ever has to be computed
  widens to `ValueSource` (`ocx_mirror_spec::value_source`). It never gains a `_from` sibling or a
  flag. There is no `cargo.env` in v1.
- **No environment model.** There is no field for a proxy, a CA, a registry mirror, a build image or
  a toolchain package (S14, S16). `platforms.<p>.runner` is the build leg's whole environment
  declaration. Same-OS cross builds are just another `runner` (S18).
- **Reused unchanged:** `platforms` (`exclude`, `min_version`, `max_version`, and `toolchain` for the
  test leg), `tests`, `metadata`, `libc_lint`, `versions`, `cascade`, `sign`, `announce`, `notify`,
  `catalog`, `build_timestamp`, `concurrency`.

---

## § 2 CLI surface

| Command | Flags (new **bold**) | New exits |
|---|---|---|
| `pipeline plan` | unchanged | 65 for every C2 refusal (`CargoError`); 69 when the index or `dl` is unavailable or redirects |
| `pipeline prepare` | `--version V`, `--plan`, `--work-dir`, `--spec`, **`--platform KEY`…** (repeatable; archive + build kinds), **`--bundles-dir DIR`** (all kinds) | 64: an entry with `build` and no `--platform`; a value outside `spec.platforms`; `--platform` on an env kind. 65: a plan without `versions[].cargo`/`build`, an unimplemented `requires`, a path-unsafe version, a 404 or oversize `.crate`. 69: `.crate` host unavailable. 78: `rustup` missing, or an ancestor cargo project. 1: a build failure, a bins mismatch, a digest mismatch |
| `pipeline push` | existing, plus **`--plan`** (required for build kinds) and **`--plan-sha256 HEX`** | 64: cargo without `--plan`. 65: a sha mismatch, a plan without `versions[].cargo`, an unimplemented `requires`, a path-unsafe version. 69: an existing-index read failure on backfill |
| `package check` | unchanged | the same refusals as `plan` |
| `package sync`, `pipeline patch` | unchanged | 64 on cargo |
| `pipeline generate ci` | unchanged | — |
| `schema plan` | unchanged | emits v5, `$id` `https://ocx.sh/schemas/plan/v5.json` |

---

## § 3 Inter-job artifacts and the hand-off layout

```
plan.json                          discover → every job      sha256 = plan_sha256
locks/                             discover → prepare        pypi only, unchanged
bundles/                           prepare → test, push      = prepare --bundles-dir / push --bundles-dir
  bundle-{V}-{slug}.tar.xz
  bundle-{V}-{slug}-metadata.json
  bundle-{V}-{slug}-build.json     build kinds only; inert to every other reader (push/bundles.rs:108-110 matches .tar.xz)
  {V}/…                            env kinds only: today's flattened subtree
junit/                             test → push               naming unchanged (plan-json.md § legs)
run-summary.json                   push → notify             + reasons build_record_missing, build_record_mismatch
```

- `{V}` is `versions[].version` verbatim. It is path-checked on read (C3), and `{slug}` is
  `platform_key_slug(key)`.
- Several prepare jobs write into one `bundles/`, merged by the renderer's artifact transport. Their
  names are disjoint, because each `(V, slug)` belongs to exactly one job.
- `--bundles-dir` overwrites same-named files, touches nothing else, and writes nothing on failure.
- This layout and `build.json` are documented in `plan-json.md`, which makes them one-way.

Work dir (per prepare process):

```
{work_dir}/{V_SLUG}/
  manifest.json                    lists this invocation's bundles (no production reader)
  {slug}/
    source/  target/  content/     removed once the bundle exists
    authoring-metadata.json  bundle.tar.xz  metadata.json
    build.json                     build kinds; written last
```

For build kinds, the `.crate` is held in memory (≤ 50 MiB) between fetch, verify and unpack. Nothing
in the task dir survives into the next invocation (S22).

---

## § 4 GitHub render

This section shows the shape, not the golden. `tests/golden/mirror-cargo.txt` pins the bytes.

**Prerequisites for every kind.** These are separate `fix:` commits that regenerate every golden:
- `test.if: ${{ !cancelled() && needs.discover.outputs.has_new == 'true' }}`.
- The test body (`generate/ci/matrix.rs:483-533`) writes each version's JUnit. It records failures
  in a counter, and exits 1 after the version loop instead of at `:530-531`.

After those commits, the cargo branch fills template slots whose non-cargo fill is the then-current
text, so non-cargo renders stay byte-identical. The new fragment `templates/prepare_cargo.yml`
matches Renovate's existing `templates/[^/]+\.yml` manager (`.github/renovate-local.json:72`).

**Workflow level.** `permissions: { contents: read }`. Push keeps its own job-level grant.

**discover**, appended to the plan step:

```sh
jq -e '[.versions[] | select((.build.requires // []) | length > 0)] | length == 0' plan.json >/dev/null \
  || { echo "::error::plan.json carries build.requires values this workflow does not implement — regenerate the workflow"; exit 1; }
echo "units=$(jq -c '. as $p | [$p.versions[] | select(.build) | . as $e | $e.platforms[]
  | {version: $e.version, platform: ., platform_slug: $p.legs[.].platform_slug, runner: $p.legs[.].runner}]' plan.json)" >> "$GITHUB_OUTPUT"
echo "plan_sha256=$(sha256sum plan.json | cut -d' ' -f1)" >> "$GITHUB_OUTPUT"
```

The step also fails when `has_new == 'true'` and `units` is `[]`, because an empty `fromJson`
matrix fails the job with an unhelpful error. `versions` stays, because the test loop reads it.

**prepare** (`templates/prepare_cargo.yml`):

```yaml
  prepare:
    needs: discover
    if: needs.discover.outputs.has_new == 'true'
    runs-on: ${{ matrix.runner }}
    permissions: { contents: read }
    strategy:
      fail-fast: false
      matrix:
        include: ${{ fromJson(needs.discover.outputs.units) }}
    steps:
      - uses: actions/checkout@<pin>
        with: { persist-credentials: false }
      - uses: ocx-sh/setup-ocx@<pin>
        with: { version: "{OCX_CLI_VERSION}", groups: default }
      - uses: actions/download-artifact@<pin>
        with: { name: plan }
      - name: Build ${{ matrix.version }} for ${{ matrix.platform }}
        shell: bash
        env: { VERSION: "${{ matrix.version }}", PLATFORM: "${{ matrix.platform }}" }
        run: |
          set -euo pipefail
          ocx-mirror package pipeline prepare{SPEC_ARG} --plan plan.json --version "$VERSION" --platform "$PLATFORM" \
            --work-dir "$RUNNER_TEMP/ocx-mirror" --bundles-dir bundles
      - uses: actions/upload-artifact@<pin>
        with: { name: "bundle-${{ matrix.version }}-${{ matrix.platform_slug }}", path: bundles/, retention-days: 1 }
```

- The job has no `secrets.*`. Matrix values reach the shell only through `env:`.
- `--work-dir "$RUNNER_TEMP/…"` keeps the build outside the checkout, so the C4 ancestor check never
  sees the mirror repository.
- A corporate cargo read token is the operator's to add, as `env:` on this one step. Docs say so; the
  render never does it.
- No `timeout-minutes` is rendered: the 360-minute default is the hosted maximum.
- `rustup` on `ubuntu-24.04-arm` and `windows-11-arm` hosted images is unverified (research gap). If
  an image lacks it, the fix is a renderer setup step on those runners. It is never part of the
  prepare command contract.

**test.** Unchanged beyond the prerequisites. Its `pattern: bundle-*` with `merge-multiple: true`
(`workflow.yml:113-117`) already merges per-platform artifacts.

**push.** It downloads `plan` beside the bundles and adds
`--plan plan.json --plan-sha256 "$PLAN_SHA256"`, with
`PLAN_SHA256: ${{ needs.discover.outputs.plan_sha256 }}` in `env:`.
- A job output cannot be rewritten by a later job.
- The `plan` artifact can be replaced by any job holding the runtime token, and upstream code on a
  hosted runner can obtain that token.

---

## § 5 Non-GitHub renders (acceptance bar)

The mirror renders none of these (owner, 2026-11-08). They are the documented shape that
`plan.json` plus `plan-json.md` must suffice for. § 5.2 runs in the acceptance harness verbatim,
and § 5.1 runs at `/e2e-test` tier 3.

### 5.1 GitLab: parent plan job + generated child pipeline

```yaml
# .gitlab-ci.yml (operator-owned, static)
plan:
  stage: plan
  script:
    - ocx-mirror package pipeline plan --format json > plan.json
    - ./ci/render-child.sh plan.json > child.yml      # operator-owned jq generator, ~40 lines
  artifacts: { paths: [plan.json, child.yml], expire_in: 1 day }
run:
  stage: run
  needs: [plan]
  trigger:
    include: [{ artifact: child.yml, job: plan }]
    strategy: depend
  variables: { PARENT_PIPELINE_ID: $CI_PIPELINE_ID }
```

The generator emits one child config. JSON is valid YAML, so `child.yml` may hold `jq` output
directly. The config contains:

| Child job | From `plan.json` | Key fields |
|---|---|---|
| `prepare:<version>` per entry without `build` | `versions[]` | the operator's default tags; rule-1 command |
| `prepare:<version>:<slug>` per (entry with `build`, p) | `versions[].platforms`, `legs[p].runner` → `tags:` | `needs: [{pipeline: $PARENT_PIPELINE_ID, job: plan}]`; rule-1 command with `--platform p` and a `--work-dir` under `$CI_BUILDS_DIR/../tmp` or similar outside the checkout; `artifacts: {paths: [bundles/]}`; `timeout:` ≥ `build.timeout_seconds` + setup (GitLab's default is 60 min); the cargo read token as a masked variable on these jobs only |
| `test:<slug>:<container>` per (P, C) | `legs[P].containers`, `legs[P].tests`, `containers[].toolchain` | `needs:` every prepare job producing P. JUnit names follow `plan-json.md`. Until `pipeline test` lands, the body is the operator's script over `ocx package test`, recording every version and failing after the loop |
| `push` | — | `needs:` every prepare and test job with `artifacts: true`, plus the parent `plan`; `when: always`; `--plan plan.json --plan-sha256 <literal baked by the generator>`; the registry credential lives in an `environment:` only this job declares |

- **Fail loud.** The generator exits non-zero when any entry carries `build.requires` (rule 5), or
  when a cargo plan's `schema_version` is below 5.
- **Trusted channel.** The sha literal sits in `child.yml`, the parent job's artifact. No child job
  can rewrite it.
- **Forge residual.** GitLab gives every job a `CI_JOB_TOKEN`, and by default that token can push to
  the project's own container registry. The scrub hides it from the children only. If `target` is
  that registry, the operator restricts the job token's registry scope, or runs the pipeline from a
  project whose token cannot write the target. Refusing to run is § Deferred to owner.
- **Limits.**
  - Everything stays in **one** child.
  - Two points are unverified (research gap): the tier of `needs:pipeline:job` from child to parent,
    and the default limit of 50 jobs per `needs` list.
  - Push's `needs` count is `new_per_run × |platforms| + Σ test jobs`, about 20 at `new_per_run: 2`
    with six platforms.
- **Self-managed runners.** They have no hosted macOS or Windows runners. An operator without one
  omits that platform from `platforms:`.

### 5.2 Cron, or one host (reference script, run by the acceptance harness)

The spec declares only the platforms this host can build. S21 applies: units run as a separate user
with a fresh `HOME`, and only the push step sees registry credentials.

```bash
#!/usr/bin/env bash
# Runs as the publishing user. BUILD runs a unit as the build user; the acceptance harness sets BUILD="" (same user).
set -uo pipefail
BUILD=${BUILD-"sudo -u ocx-build env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=$(sudo -u ocx-build mktemp -d)"}
ocx-mirror package pipeline plan --format json > plan.json || exit $?
[ "$(jq -r .has_new plan.json)" = true ] || exit 0
jq -e '[.versions[] | select((.build.requires // []) | length > 0)] | length == 0' plan.json >/dev/null || exit 65
PLAN_SHA256=$(sha256sum plan.json | cut -d' ' -f1); chmod 0444 plan.json
rc=0
while IFS=$'\t' read -r v p; do
  $BUILD ocx-mirror package pipeline prepare --plan plan.json --version "$v" ${p:+--platform "$p"} \
    --work-dir "$PWD/work" --bundles-dir "$PWD/bundles" || rc=1      # keep going: push reports what is missing
done < <(jq -r '.versions[] | select(.kind != "metadata-drift") | . as $e
  | if .build then (.platforms[] | [$e.version, .]) else [$e.version, ""] end | @tsv' plan.json)
$BUILD ./run-tests.sh plan.json "$PWD/bundles" "$PWD/junit" || rc=1   # operator-owned until `pipeline test`
ocx-mirror package pipeline push --plan plan.json --plan-sha256 "$PLAN_SHA256" \
  --bundles-dir "$PWD/bundles" --junit-dir "$PWD/junit" --write-summary run-summary.json
push_rc=$?
exit $(( push_rc != 0 ? push_rc : rc ))
```

- **Keeps going.** A failed unit or test never skips push. Push owns the verdict (`missing_bundle`,
  `Partial`), and the script returns push's code or else the aggregate unit failure.
- **The isolation is the operator's.** The `ocx-build` user owns `work/`, `bundles/` and `junit/`.
  It cannot write `plan.json`, the publishing user's `HOME`, or the `ocx`/`ocx-mirror` binaries. A
  per-unit `podman run --rm` is the equivalent with a container.
- **The cargo read token is the build user's environment.** It belongs in the `env -i` list, never
  in a `credentials.toml` (Decision 3 (iv)).

### 5.3 Jenkins

A scripted pipeline runs `parallel` over the same `(version, platform)` projection. Each branch runs
on `node(legs[p].runner.join(' && '))` with `catchError(buildResult: 'FAILURE')`, so a failed
branch never skips push. Each branch stashes `bundles/`, and test branches are grouped by (P, C) in
the same way. The push stage unstashes everything, binds the registry credential with
`withCredentials` in that stage only, and passes `--plan-sha256` from the plan stage. Push needs its
own agent, or S21's user split on a shared one.

---

## § 6 Testability (every rule observable without reading code)

| Contract | Input | Expected observable outcome | Seam |
|---|---|---|---|
| C1 | each `tests/fixtures/invalid/cargo_*.yml` (incl. `bins: [con]`) | `validate` exits 65; stderr names the field, never a secret value | `tests/spec_validation.rs` |
| C1 | an archive spec with `annotations: {sh.ocx.mirror.x: y}` | exit 65 | same |
| C1 | a cargo spec whose `platforms.<p>.tests: []` and has no top-level `tests` | exit 65 at `validate` | same |
| C2 discovery | an index with an unparseable line; a wrong-case `name`; a non-semver `vers`; a 63-hex `cksum`; a `vers` listed twice; yanked entries | 65 ×5; yanked dropped | unit, local HTTP fixture |
| C2 discovery | `config.json` 404; without `dl`; index 404; 503; 401; 9 MiB body; a 302 from the index host | 65 / 65 / 65 / 69 / 69 / 69 / 69 (names the `Location` host) | unit |
| C2 `dl` | templates with each marker; with none; one rendering `http://` or `https://u@h` | the documented URLs; 65 | unit |
| C2 fetch | `OCX_AUTH_<index>_TOKEN` + a netrc `default` entry; a `dl` on a third host | no `Authorization` on the third-host request; present on the index host | unit, recording server |
| C2 fetch | a `.crate` 302 → `http://`; → a host resolving to `127.0.0.1`; → six hops; → a public host | refused ×3 (69); followed without credentials | unit |
| C2 fetch | a 51 MiB `.crate`; a body cut mid-stream | 65 / 69; nothing written | unit |
| C2 scan/unpack | `.crate`s holding: a symlink; a hardlink; a char device; a FIFO; a PAX global header; `../x`; an absolute path; two `Cargo.toml`; invalid TOML; a 1.1 GiB gzip bomb; 100 001 entries | each 65, at `plan`/`check` (scan) and at `prepare` (unpack) | unit + acceptance |
| C2 unpack | a `.crate` with `.cargo/config.toml` and `rust-toolchain.toml` at the package root, and `src/.cargo/x` deeper | the first two absent on disk with a warning naming them; the deeper path present | unit |
| C2 bins | lib-only; bins all gated by an unrequested feature; no `Cargo.lock`; `rust-version` 1.90 with channel 1.89.0; implicit `src/bin/x.rs` | 65 ×4; `x` expected | unit (via `cargo_toml`) + acceptance |
| C2 closure | `features` closure over `default`, `dep:x`, `x/y` | the expected bin set exactly | unit (table) |
| C3 | an archive spec with two new versions | no `build` on either entry; same jobs as v4's rule | unit + `plan_legs_match_the_rendered_matrix` |
| C3 | a cargo spec with three platforms, one new version, one drift entry | `build.timeout_seconds` on the new entry only; `schema_version` 5; `schema plan` validates the output | unit |
| C3 | a platform with `toolchain: {project: ocx.toml}` and no containers | `containers[0].toolchain == {project: "ocx.toml", groups: ["default"]}`; equals the rendered matrix leg | parity test |
| C3 / A29 | a plan entry with `build.requires: ["cross"]` | `prepare` and `push` exit 65 before any fetch or registry call; the discover step exits 1 | acceptance + rendered-script fixture |
| C3 | `versions[].version` = `../x` or `a/b`; a platform not in `spec.platforms` | `prepare` and `push` exit 65 | unit |
| C3 | a v4 `plan.json` fed to `prepare --plan` for an archive spec | succeeds unchanged | acceptance |
| C4 | cargo `prepare` without `--platform`; with `--platform freebsd/amd64`; pypi `prepare --platform linux/amd64` | 64 ×3; the first message names `versions[].build` | acceptance |
| C4 | an archive `prepare --platform linux/amd64` | only that bundle; `manifest.json` lists only it | acceptance |
| C4 | `--bundles-dir` after a cargo prepare | exactly three files per task, accepted by `push --bundles-dir` unchanged | acceptance |
| C4 / S22 | a second cargo prepare over a work dir holding a bundle and `build.json` from the first | the old files are deleted first; the build runs again | acceptance |
| C4 | `--work-dir` inside a directory holding `Cargo.toml` (and separately `.cargo/config.toml`) | exit 78 naming the path; no child spawned | acceptance |
| C5 | argv/env builder with each grammar-rejected token (`-x`, `/`, whitespace, `;`, NUL) | unconstructible (the newtype refuses) | property test |
| C5 env | env with `GH_TOKEN`, `GITHUB_ENV`, `CI_REPOSITORY_URL`, `COSIGN_KEY`, `MY_SECRET`, `OCX_AUTH_X_TOKEN`, a `sign:` `env://K`, `CARGO_REGISTRIES_X_TOKEN` | all but the last absent from every child; the last present in `fetch` only | unit |
| C5 env | the install child | has `CARGO_NET_OFFLINE=true`; argv has `--offline`, `--path=`, `--root=`, `--target=` | unit |
| C5 rustflags | msvc with `CARGO_ENCODED_RUSTFLAGS=a`; with `RUSTFLAGS="-C opt-level=2"`; with neither | `a␟-C␟target-feature=+crt-static`; `-C opt-level=2 -C target-feature=+crt-static`; `--config=target.<triple>.rustflags=[…]` on argv | unit |
| C5 | darwin with and without an operator `MACOSX_DEPLOYMENT_TARGET` | the operator's value; else `11.0` / `10.12` | unit |
| C5 relay | a child printing `a\r::add-mask::x` on stdout and `\x1b[31mred` on stderr | log lines `cargo\| a::add-mask::x` and `cargo\| [31mred`, with no control byte | unit |
| C5 | `PATH` without `rustup` | exit 78 naming the docs page | acceptance |
| C5 | a crate that fails to compile; timeout 60 s on a slow build | exit 1 with the prefixed tail / the timeout | acceptance |
| C5 bins | `content/bin/` lacking one planned bin (and holding one unplanned), with `metadata:` declaring the planned set | exit 1 naming the missing and the unexpected bin, before `ocx package create` runs (`--bin-scan verify` alone would accept it) | unit over the check, with a fixture tree |
| C5 | built `content/` | no `.crates.toml` or `.crates2.json`; `bin/` equals the plan's `bins` | acceptance |
| C5 / S11 | `$CARGO_HOME/config.toml` with `[source.crates-io] replace-with` pointing at a local registry; the crate ships `.cargo/config.toml` replacing the source elsewhere; crates.io unreachable | the build succeeds, with deps from the operator's replacement | acceptance |
| C6 | cargo `push` without `--plan`; `--plan-sha256` of another file | 64; 65 before any registry call | acceptance |
| C6 | every build leg red (no bundles dir at all) | every planned pair `missing_bundle`; `run-summary.json` written; exit by verdict | acceptance |
| C6 | one planned platform's bundle removed; an extra bundle for an unplanned version | `Partial`, `missing_bundle`, no cascade for V; the unplanned bundle not pushed, with a warning | acceptance |
| C6 | a bundle byte-flipped; `build.json` deleted; a `build.json` copied from another platform; one with an extra field; one with `rustc: "x\u0007"` | `build_record_mismatch`; `build_record_missing`; `mismatch`; `missing`; `mismatch` | acceptance |
| C6 annotations | run 1 pushes linux with features `[a]`; run 2 backfills darwin with features `[b]` | after run 2: `…cargo.features = mixed`; matching keys keep their value | acceptance (registry inspect) |
| C6 annotations | two legs reporting different `rustc` lines | `…cargo.rustc = mixed` | acceptance |
| C7 | each error path above | the stated exit code | covered by the rows above |
| C8 | all six keys ± libc; `darwin/arm64+libc.musl` | the table's triple; 65 | unit (total-function test) |
| C9 | `generate ci` over `tests/fixtures/mirror-cargo.yml` | golden matches; `runs-on: ${{ matrix.runner }}` on prepare; `persist-credentials: false`; `--work-dir "$RUNNER_TEMP/…"`; `!cancelled()` on test; `grep -c 'secrets\.'` = 0 between `prepare:` and `test:` | golden |
| C9 prereq | a rendered test body run against two versions, the first failing | both JUnit files written; the step exits 1 after the loop | rendered-script fixture |
| C9 | every existing fixture after the prerequisite commits | byte-identical goldens | golden |
| § 5.2 | the reference script against the harness registry and local index, with one unit forced red | push runs; `run-summary.json` shows `missing_bundle`; the script exits non-zero | acceptance |
| § 5.1 | a GitLab child config generated from the acceptance `plan.json` | runs build → test → push and pushes; a red leg reads `Partial` | `/e2e-test` tier 3 |

---

## § 7 What this spec does not decide

- **Tier 2 (post-process) and tier 3 (recipe).** Only their seams are named here: the window row
  above, and the `Option` → enum change.
- **Cross-OS builds, container builds and the vendor split.** All are follow-ups behind
  `build.requires` values.
- **`pipeline test` as one command.** § Deferred to owner.
- **The operator's GitLab, Jenkins or cron files.** § 5 is their documented shape, not a mirror
  output.
- **Build caching.** `Swatinem/rust-cache` keyed on crate + channel + triple is the documented
  upgrade.
- **The per-platform `rustc` seat in ocx** (OQ3), and phases 2–4 (variants, `rebuild`, SLSA).

## § 8 Open items

- **OQ1–OQ3** are owned by the ADR: the read credential on build legs, the `plan.json` shape, and
  toolchain truth. This spec assumes each recommended answer. Each alternative changes only the rows
  that name it:
  - (v) adds a vendor job and drops `cargo fetch` from build legs;
  - C or B′ replaces the discover projection and § 5's generator input;
  - a pin requirement adds one C1 rule.
- **Deferred to owner** (ADR): splitting the renderer contract into its own ADR; `pipeline test` in
  v1; refusing to run on a `CI_JOB_TOKEN` + `CI_REGISTRY` target; an authenticated bundle-digest
  channel.
- **Research gaps**, settled by the rollout rather than by an owner:
  - the tier of GitLab `needs:pipeline:job` and the default `needs` limit;
  - `rustup` on the `ubuntu-24.04-arm` and `windows-11-arm` hosted images;
  - Rosetta on arm64 macOS hosted images;
  - the `cargo_toml` dependency footprint.

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-09-17 | Architect (hex-arch-cargo-20260917) | Initial design spec |
| 2026-11-08 | Architect (hex-architect revision) | Rewritten against the post-split crates and plan.json v5; `MirrorTask.build` seam replaces `Acquire`; `--path` build over the verified `.crate`; rustup in prepare; `--bundles-dir` hand-off; credential classes per job; GitLab, cron and Jenkins renders (§ 5); testability table (§ 6) |
| 2026-11-08 | Architect (hex-architect Round 1 fixes) | `versions[].build` marker replaces `prepare[]`; `Option<CargoBuild>`; fetch/offline split; package-root config skipped + ancestor check; bounded fetch/unpack with entry types, host-gated creds, redirect policy; bins check; `request_sha256`; `mixed` annotations; no build resume; prerequisite test-job fixes; `containers[].toolchain`; single-host reference script with isolation and aggregate status; § 6 extended |
