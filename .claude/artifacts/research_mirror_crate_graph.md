# Research: ocx-mirror module dependency graph for the crate split

## Metadata

- Date: 2026-09-22
- Domain: architecture
- Triggered by: `.agents/discussions/bazel-crate-split.md`
- Expires: 2027-03-22

All line counts via `wc -l`, tests included and (where present) broken out
separately. All file/module facts gathered read-only via `grep`/`find`
against the working tree; no `cargo` build was run.

**Tooling note**: plain `find`/`grep` in this shell resolve to an `rtk`
proxy that silently truncates/summarizes output on larger file sets
(e.g. reported 15 files for `src/command/**/*.rs` instead of 93). All
counts in this document used `/usr/bin/find` and `/usr/bin/grep` directly
after that was caught. `zsh` also aborts a whole compound command when an
unquoted glob for a single-file "module directory" (e.g. `src/annotations/*.rs`)
matches nothing — this produced a false "no `#[cfg(test)]`" reading for
every single-file module on the first pass; corrected using `find`-based
file lists instead of globs.

---

## 1. Top-level module inventory

Declared in `src/lib.rs` (13 `mod`, 2 `pub mod`, 1 `#[cfg(test)] mod`) and
`src/main.rs` (1 `mod tracing_init`, binary-only, not part of the library).

| Module | Visibility | Total lines (incl. tests) | Test-file lines (`tests.rs`/`tests/*`) | Inline `#[cfg(test)]` present? |
|---|---|---:|---:|---|
| `pipeline` | `mod` | 28,384 | 12,434 | yes (23 files) |
| `command` | `pub use Command` re-export | 23,503 | 12,913 | yes (11 files) |
| `spec` | `pub mod` | 16,013 | 7,327 | yes (18 files) |
| `source` | `mod` | 2,099 | 359 | yes (5 files) |
| `filter` | `mod` | 1,165 | 0 (inline only) | yes, inline (897 of 1,165 lines) |
| `discord` | `mod` | 871 | 0 (inline only) | yes, inline (548/871) |
| `run_summary` | `mod` | 563 | 0 (inline only) | yes, inline (319/563) |
| `error` | `pub mod` | 528 | 0 (inline only) | yes, inline (248/528) |
| `http` | `mod` | 503 | 0 (inline only) | yes, inline (322/503) |
| `auth` | `mod` | 494 (266 + `auth/tests.rs` 228) | 228 | yes, `mod tests;` pointer |
| `junit` | `mod` | 474 | 0 (inline only) | yes, inline (232/474) |
| `resolver` | `mod` | 367 | 0 (inline only) | yes, inline (268/336 in `resolver.rs`; `asset_resolution.rs` 31 lines untested) |
| `version_platform_map` | `mod` | 175 | 0 (inline only) | yes, inline (99/175) |
| `normalizer` | `mod` | 162 | 0 (inline only) | yes, inline (80/162) |
| `annotations` | `mod` | 219 | 0 (inline only) | yes, inline (96/219) |
| `test_support` | `#[cfg(test)] mod` | 98 | n/a (itself test-only) | n/a |
| `tracing_init` | binary-only `mod` (in `main.rs`) | 216 | 0 (inline) | yes, inline |

Crate total: 75,452 lines across `src/`. `pipeline` + `command` + `spec`
are 68,000 of those lines (≈90%) and are exactly the three modules with
the direct-cycle back-edges (§2).

Second-level structure:
- `command/`: `schema.rs` (feature-gated), `dist/` (3 files), `package/`
  (`mod.rs`, `check.rs`, `options.rs`, `sync.rs`, `validate.rs`,
  `pipeline/` — 12 leaf files + `generate/` with `ci/` (matrix/drift/slot/
  permissions/aux_workflows + 14 test files) — this is where most of
  command's 93 files and 12,913 test lines live), `registry/` (3 files).
- `pipeline/`: 17 top-level files (`download.rs`, `lock_derive.rs`,
  `mirror_result.rs`, `mirror_task.rs`, `ocx_cli.rs`, `orchestrator.rs`,
  `package.rs`, `progress.rs`, `push.rs`, `python_prepare.rs`,
  `python_push.rs`, `registry_copy.rs`, `registry_sync.rs`,
  `sign_backfill.rs`, `target_registry.rs`, `verify.rs`, `dist_sync.rs`)
  plus 7 subtrees: `dist_sync/`, `ocx_cli/`, `orchestrator/`, `push/`,
  `registry_copy/`, `registry_sync/` (largest — `cache`, `catalog`,
  `destination`, `glob`, `index_write`, `plan`, `report`), `sign_backfill/`.
- `spec/`: 30 top-level files (per-config-key: `assets.rs`,
  `asset_type.rs`, `bin_scan.rs`, `cascade_config.rs`, `catalog_config.rs`,
  `concurrency_config.rs`, `dist.rs`, `forge.rs`, `load.rs`,
  `metadata_config.rs`, `notify_config.rs`, `ocx_mirror_config.rs`,
  `platform_keys.rs`, `platforms_config.rs`, `prescan.rs`,
  `python_config.rs`, `registry.rs`, `sign_config.rs`, `source.rs`,
  `strip_components_config.rs`, `target.rs`, `tests_config.rs`,
  `validate.rs`, `value_source.rs`, `variant.rs`, `verify_config.rs`,
  `versions_config.rs`, `wheels.rs`) plus `dist/`, `prescan/`, `registry/`,
  `sign_config/`, `tests/` subtrees.
- `source/`: flat — `generator.rs`, `github_release.rs`, `pylock.rs`,
  `pypi.rs` (+ `pypi/tests.rs`), `url_index.rs`.

---

## 2. Intra-crate edge list (top-level granularity)

Edge = "module A's non-test-only code has at least one `crate::B::…`
reference." Counts are raw `crate::`-prefixed reference occurrences
(not deduplicated symbols); `test_support` is a `#[cfg(test)]`-only
module so edges into it are test-only and not load-bearing for a
production split.

| From \ To | pipeline | spec | command | error | source | run_summary | resolver | filter | http | annotations | auth | discord | normalizer | version_platform_map | junit |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| **command** | 59 | 39 | (self 32) | 28 | 12 | 11 | 8 | 7 | 6 | 5 | – | 3 | 3 | 2 | 1 |
| **pipeline** | (self 23) | 30 | 4 | 15 | – | 2 | – | 2 | 6 | 5 | 4 | – | – | 1 | – |
| **spec** | 13 | (self 23) | – | 17 | 3 | – | – | 7 | 3 | 1 | 3 | – | – | – | – |
| **source** | 1 | 5 | – | 6 | (self 1) | – | – | – | 5 | – | 1 | – | – | – | – |
| **filter** | – | 1 | – | – | – | – | 1 | (self) | – | – | – | – | – | 1 | – |
| **discord** | – | – | – | 1 | – | – | – | – | 2 | – | – | (self) | – | – | – |
| **http** | – | – | – | 1 | – | – | – | – | (self 2) | – | – | – | – | – | – |
| **junit** | – | – | – | 3 | – | – | – | – | – | – | – | – | – | – | (self) |
| **normalizer** | – | 1 | – | – | – | – | – | – | – | – | – | – | (self) | – | – |
| **resolver** | – | – | – | – | 1 | – | (self) | – | – | – | – | – | – | – | – |
| **auth** | – | – | – | 1 | – | – | – | – | – | – | (self) | – | – | – | – |
| **error** | – | – | – | (self) | – | – | – | – | 0¹ | – | – | – | – | – | – |
| **run_summary**, **annotations**, **version_platform_map** | production-code-empty (only test-only edges to `test_support`) |

¹ `error.rs:184` has one *doc-comment* link `` [`crate::install_extra_roots`] `` (an item defined in `http.rs`, re-exported at the crate root). No compiled code dependency — not a real edge, see cycle note below.

### Second-level edges (pipeline submodule ↔ pipeline submodule, non-test)

| From submodule | Uses (same-tree `crate::pipeline::X` / `super::X`) |
|---|---|
| `dist_sync` | `verify`, `registry_sync`, `mirrored_url` (own), `download`, `upload` (own) |
| `ocx_cli` | `forward_ocx_env` (own), `sign` (own) |
| `orchestrator` | `verify`, `push`, `progress`, `package`, `ocx_cli`, `mirror_task`, `mirror_result`, `download` |
| `push` | `push_and_cascade` (own), `ocx_cli`, `mirror_task`, `mirror_result` |
| `registry_copy` | `registry_sync` (for `is_legal_oci_tag`) |
| `registry_sync` | `index_write`, `destination`, `cache`, `registry_copy`, `options`, `glob`, `catalog` (own-tree) |
| `sign_backfill` | `ocx_cli` |
| `target_registry` | `sign_backfill` |
| `python_prepare` | `orchestrator`, `progress`, `download` |
| `python_push` | `ocx_cli`, `python_prepare`, `target_registry` |
| `lock_derive` | `ocx_cli` |
| `download`, `mirror_result`, `mirror_task`, `package`, `progress`, `verify` | none (leaves) |

### Second-level edges (`command` submodule → `pipeline` submodule it drives)

| `command` submodule | `pipeline` submodules it calls |
|---|---|
| `command::package` | `ocx_cli` (23), `python_prepare` (8), `target_registry` (6), `orchestrator` (6), `sign_backfill` (2), `python_push` (2), `mirror_task` (2), `mirror_result` (2), `registry_copy` (1), `lock_derive` (1), `download` (1) |
| `command::registry` | `registry_sync` (3) |
| `command::dist` | `dist_sync` (2) |

`source/` submodules do not reference each other (`generator.rs` has one
doc-comment mention of `url_index::from_generator`, not a real edge).

### Cycles at top-level granularity

Four real (compiled, non-test) 2-node cycles, plus one false positive.

| Cycle | A→B evidence | B→A evidence |
|---|---|---|
| **`command` ↔ `pipeline`** | `pipeline` submodules import `command`-owned option/format types: `crate::command::package::options::OutputFormat` (`src/pipeline/registry_sync/report.rs:14`, `src/pipeline/dist_sync/report.rs:14`), `crate::command::registry::options::RegistrySyncOptions` (`src/pipeline/registry_sync.rs:72`) | `command::package`/`registry`/`dist` drive `pipeline::{ocx_cli,orchestrator,registry_sync,dist_sync,…}` (59 refs, e.g. `src/command/package/mod.rs`, `src/command/registry/sync.rs`) |
| **`spec` ↔ `pipeline`** | `spec/registry.rs:24-26` imports `crate::pipeline::registry_sync::{catalog::index_host, destination::DestinationTemplate, glob::Glob}`; `spec/dist.rs:441-448` calls `crate::pipeline::dist_sync::layout::{LayoutTemplate, check_plain_path, SnapshotTemplate}` | `pipeline` uses `spec` types pervasively for config (30 refs, e.g. `RegistrySource`, `RegistrySpec`, `OnError` in `src/pipeline/registry_sync.rs:75`) |
| **`spec` ↔ `source`** | `spec/source.rs:11` `use crate::source::url_index::IndexAsset;`; `spec/source.rs:281` `crate::source::pypi::DEFAULT_INDEX`; `spec/value_source.rs:144` `crate::source::generator::run(...)` | `source/url_index.rs:11` `use crate::spec::GeneratorConfig;`, `:133,169` `crate::spec::UrlIndexVersion`; `source/generator.rs:17` `use crate::spec::GeneratorConfig;` |
| **`spec` ↔ `filter`** | `spec/versions_config.rs:222,300`, `spec/platforms_config.rs:242,248`, `spec.rs:486` call `crate::filter::{within_bounds, within_bounds_ex, version_cmp}` | `filter.rs:9` `use crate::spec::{BackfillOrder, ResolvedBounds, VersionsConfig};` |
| ~~`error` ↔ `http`~~ (not a real cycle) | `http.rs:49` `use crate::error::MirrorError;` (real) | `error.rs:184` is only an intra-doc link `` [`crate::install_extra_roots`] `` — no compiled dependency. `error` has **zero** real edges out. |

**Implication**: `command`, `pipeline`, `spec`, `source`, and `filter` form
one strongly-connected cluster today. A workspace split needs either (a) a
shared "spec types" crate that `pipeline`/`source`/`filter`/`command` all
depend on downward (breaking `spec→pipeline`, `spec→source`, `spec→filter`
by moving the *pipeline-side* helper types spec currently reaches into —
`registry_sync::{catalog,destination,glob}`, `dist_sync::layout`,
`source::{url_index,pypi,generator}`, `filter::{within_bounds,version_cmp}`
— into that shared crate or into `spec` itself), or (b) trait-based
inversion (spec defines a trait, pipeline/source/filter implement it),
which is a real design decision, not a mechanical move.

---

## 3. `error.rs` / `MirrorError` consumers

| Module | `crate::error::MirrorError` refs | Other `crate::error::*` |
|---|---:|---|
| `command` | 28 | – |
| `spec` | 17 | – |
| `pipeline` | 13 | 1 (`sign_exit_code`) |
| `source` | 6 | – |
| `junit` | 3 | – |
| `auth` | 1 | – |
| `discord` | 1 | – |
| `http` | 1 | – |

**Modules that do NOT touch `MirrorError` or any `spec` type** — genuinely
generic, confirmed by both the edge list (§2) and by grep for
`Result<`/`enum …Error`/`anyhow` inside each:

| Module | Error/Result style | Verdict |
|---|---|---|
| `resolver` | no `Result`/`Error` at all (pure struct/fn) | clean |
| `run_summary` | no `Result`/`Error` at all | clean |
| `annotations` | no `Result`/`Error` at all | clean |
| `version_platform_map` | no `Result`/`Error` at all | clean |
| `normalizer` | `anyhow::Result` (generic, not `MirrorError`) | clean |
| `filter` | no `Result`/`Error`; **but** depends on `ocx_python::uv_pep440::Version` directly (`src/filter.rs:162-163,260`) and on `crate::spec::{BackfillOrder,ResolvedBounds,VersionsConfig}` (cycle, §2) | not spec/error-clean, but MirrorError-clean |

`http`, `auth`, `discord`, `junit` DO use `MirrorError` directly in their
public function signatures (`pub fn install_extra_roots() -> Result<(),
TlsError>` is the one exception — `http.rs:72` returns `TlsError`, not
`MirrorError`; `client()`/others do return `MirrorError`). These four are
not automatically extractable as "no error-type dependency" crates; they'd
need either `MirrorError` moved to a shared crate they can depend on, or
their own local error types with `MirrorError` built by `From`/mapping at
the `pipeline`/`command` boundary (the latter matches the existing pattern
already used for `ocx_python`'s own `LockError`/`SelectError`/etc., see §5).

---

## 4. External crate usage per top-level module

Cargo.toml deps (root crate): `ocx_config`, `ocx_console`, `ocx_exit`,
`ocx_index`, `ocx_oci`, `ocx_package`, `ocx_sign`, `ocx_util`, `ocx_python`
(path dep on `crates/ocx_python`), `tokio`, `clap`, `anyhow`, `bytes`,
`futures`, `serde`, `serde_json`, `serde_yaml_ng`, `schemars` (optional,
`jsonschema` feature), `chrono`, `regex`, `url`, `octocrab`, `http`,
`tower-service`, `quick-junit`, `reqwest`, `rustls`, `hex`, `tracing`,
`tracing-subscriber`, `log`, `tempfile`.

| Module | External crates actually used |
|---|---|
| `annotations` | `ocx_oci`, `ocx_util`, `chrono` |
| `auth` | `ocx_oci`, `ocx_util`, `url`, `reqwest`, `tempfile` |
| `command` | `ocx_config`, `ocx_console`, `ocx_exit`, `ocx_index`, `ocx_oci`, `ocx_package`, `ocx_util`, `ocx_python`, `tokio`, `clap`, `futures`, `serde`, `serde_json`, `serde_yaml_ng`, `schemars`, `chrono`, `regex`, `url`, `http`, `rustls`, `tracing`, `log`, `tempfile` |
| `discord` | `tokio`, `serde`, `serde_json`, `http`, `reqwest`, `rustls`, `tracing` |
| `error` | `ocx_config`, `ocx_exit`, `anyhow` (the `ocx_python` hit is a doc-comment mention only, not a real dep) |
| `filter` | `ocx_oci`, `ocx_package`, `ocx_python` (real: `uv_pep440::Version`), `url` |
| `http` | `ocx_config`, `ocx_util`, `octocrab`, `http`, `reqwest`, `rustls`, `log` |
| `junit` | `tokio`, `quick_junit`, `tempfile` |
| `normalizer` | `ocx_package`, `anyhow`, `chrono` |
| `pipeline` | `ocx_config`, `ocx_console`, `ocx_exit`, `ocx_index`, `ocx_oci`, `ocx_package`, `ocx_sign`, `ocx_util`, `ocx_python`, `tokio`, `anyhow`, `bytes`, `futures`, `serde`, `serde_json`, `serde_yaml_ng`, `regex`, `url`, `http`, `reqwest`, `rustls`, `hex`, `tracing`, `log`, `tempfile` |
| `resolver` | `ocx_oci`, `regex`, `url` |
| `run_summary` | `serde`, `serde_json` |
| `source` | `ocx_oci`, `ocx_package`, `ocx_python`, `tokio`, `anyhow`, `serde`, `serde_json`, `schemars`, `regex`, `url`, `octocrab`, `http`, `tower_service`, `reqwest`, `rustls`, `log`, `tempfile` |
| `spec` | `ocx_config`, `ocx_exit`, `ocx_oci`, `ocx_package`, `ocx_util`, `ocx_python`, `tokio`, `anyhow`, `serde`, `serde_json`, `serde_yaml_ng`, `schemars`, `regex`, `url`, `http`, `reqwest`, `log`, `tempfile` |
| `version_platform_map` | `ocx_oci`, `ocx_package` |
| `tracing_init` (binary-only) | `ocx_console`, `clap`, `tracing_subscriber` |

`pipeline` and `spec` each pull in nearly the entire dependency surface
(25 and 18 of ~31 deps respectively) — a mechanical Cargo.toml split per
future crate is easy for the leaf modules (`resolver`, `run_summary`,
`annotations`, `version_platform_map`, `normalizer`, `junit`, `discord`,
`auth`) but `pipeline`/`spec`/`command` will each still carry a large,
overlapping dependency set regardless of how the cycles in §2 are broken.

---

## 5. `crates/ocx_python`

**Cargo.toml** (`crates/ocx_python/Cargo.toml`): `ocx_oci`, `ocx_package`
(both path deps into `external/ocx`), `serde`, `serde_json`, `sha2`,
`toml`, `thiserror`, `tar`, `zstd`, `zip`, plus 4 pinned git deps to one
`astral-sh/uv` rev: `uv-distribution-filename`, `uv-platform-tags`,
`uv-pep508`, `uv-pep440`. `[lints.rust] warnings = "deny"` set
independently (own package, not covered by root manifest's lint table).

**Layout**: `src/{lib,collide,compose,error,lock,naming,platform,repack,select}.rs`
— 3,976 lines total including inline tests (all 7 non-lib modules carry
`#[cfg(test)]`). `tests/fixtures/`: `generate.py`, a `pylock/` dir (5 TOML
fixtures), a `wheels/` dir (8 real `.whl` files used by repack/collide
tests), and a `README.md`.

**Public API surface** (`lib.rs`): `pub mod` for all 7 submodules, plus a
curated re-export list — `uv_pep440`, `uv_distribution_filename`,
`CollisionError`/`check_collisions`, `ComposeError`/`EntrypointSelection`/
`EnvComposition`/`EnvSpec`/`WheelLayer`/`compose_env`, `LockError`/
`LockedPackage`/`LockedWheel`/`Pylock`/`parse_pylock`,
`WheelReference`/`WheelScope`/`normalize_package_name`/`wheel_reference`,
platform types, repack types, `SelectError`/`WheelRef`/`select_wheels`.

**Mirror-concept leakage check**: `grep -rn "MirrorError|ocx_mirror|crate::spec"
crates/ocx_python/src/` → zero real hits (the only matches are doc
comments in `lib.rs`/`error.rs` *describing* how the mirror consumes this
crate's errors, e.g. `//! The mirror wraps each public error in a
MirrorError variant`). **Confirmed clean** — this crate is already
decoupled and is the strongest "promote to ocx as a published `ocx_python`
library" candidate as-is.

### `src/source/pylock.rs` (225 lines, mirror-side adapter)

- Purpose: `source.type: pylock` adapter — reads a committed
  `pylock.toml`, returns the single locked app version as a `VersionInfo`.
- Imports: `ocx_package::version::Version` (ecosystem), `ocx_python::{normalize_package_name, LockError, LockedPackage, Pylock}` (translation lib), `super::VersionInfo` (mirror `source` type), `crate::error::MirrorError` (mirror error type).
- Callers: consumed via `source::pylock::{load, list_versions, app_version, find_app_package, classify_error}` — not grepped as call sites here since it's the leaf; its public fns are called from `command/package/pipeline/{plan,prepare}.rs` and `pipeline/lock_derive.rs` indirectly through the `source` dispatch, plus directly wherever a pylock-classified error needs `classify_error`.
- Verdict: this file is 100% mirror orchestration glue (spec-dir resolution, `MirrorError` classification) around the pure `ocx_python::parse_pylock`/`LockError` primitives — correctly placed in the mirror crate, not a candidate to move.

### Every `ocx_python::` call site in `src/` (23 files hit; grouped, doc-comment-only files excluded from the "real dependency" set)

| File | Real code use? |
|---|---|
| `src/source/pylock.rs` | yes — `parse_pylock`, `normalize_package_name`, `LockError`, `LockedPackage`, `Pylock` |
| `src/source/pypi.rs:111,202` | yes — `normalize_package_name`, `uv_distribution_filename::DistFilename` |
| `src/pipeline/lock_derive.rs:28,432` | yes — `Pylock`, `parse_pylock` |
| `src/pipeline/python_prepare.rs` (10 sites) | yes — `PythonTarget`, `WheelScope`, `EntrypointSelection`, `repack_wheel`, `check_collisions`, `EnvSpec`, `compose_env`, `TargetPlatform`, `VariantConstraints`, `InterpreterPin`, `Implementation` |
| `src/spec/python_config.rs` (7 sites) | yes — `EntrypointSelection` variants (drives spec→translation-lib mapping) |
| `src/spec/wheels.rs:175` | comment only |
| `src/spec.rs:102` | comment only |
| `src/filter.rs:162,163,260` | yes — `uv_pep440::Version` directly (see §3) |
| `src/error.rs:52` | comment only |
| `src/command/package/pipeline/plan.rs`, `plan/env.rs`, `prepare.rs`, `describe.rs` | yes — `select_wheels`, `Pylock`, `WheelScope`, `PythonTarget`, `wheel_reference`, `read_wheel_description`, `LockedWheel`, `WheelDescription` |

### `python_prepare.rs` / `python_push.rs` / `lock_derive.rs` / `spec/python_config.rs` / `spec/wheels.rs` — generic or mirror-specific?

| File | Lines | Verdict |
|---|---:|---|
| `pipeline/python_prepare.rs` | 751 | Mirror orchestration — builds `MirrorError`-typed pipeline steps around `ocx_python::{compose_env, repack_wheel, check_collisions}`; not portable as-is |
| `pipeline/python_push.rs` | 796 | Mirror orchestration — drives `ocx_cli`/`target_registry` push of the composed env; mirror-only |
| `pipeline/lock_derive.rs` | 1,031 | Mirror-specific: derives a `pylock.toml` from PyPI metadata + `requires-python` heuristics (two `LazyLock<Regex>` statics) for the mirror's `source.type: pypi` flow; calls `ocx_python::parse_pylock` only to self-validate its own derived output. Not a generic translation-library concern — this is "how the mirror discovers versions," not "how a lock becomes an OCX package." |
| `spec/python_config.rs` | 394 | Mirror spec schema (`EntrypointsConfig`, YAML `#[derive(Deserialize)]`), maps into `ocx_python::EntrypointSelection`. Mirror-owned by construction (it's spec-format code). |
| `spec/wheels.rs` | 373 | Mirror spec schema (`wheels:` filter-entry parsing, `FILTER_ENTRY_RE` static), feeds into `ocx_python::select_wheels`'s target — mirror spec, not portable. |

None of these five are misplaced; they are the mirror's use of the
already-generic `ocx_python` library, correctly kept out of it.

---

## 6. Tests, fixtures, generated assets

- **Inline `#[cfg(test)]`**: present in every top-level module except
  `test_support` itself (16 of 17 library modules; see §1 table).
- **`tests.rs` split-out files**: `auth/tests.rs` and dozens under
  `command/`, `pipeline/`, `spec/`, `source/` subtrees (see the `find`
  listing — pattern is `<module>/tests.rs` + `<module>/tests/<case>.rs`
  for large modules like `registry_sync`, `push`, `plan`, `prescan`).
- **`src/test_support.rs`** (98 lines, `#[cfg(test)]`-gated in `lib.rs`):
  exposes `pub(crate) static OCX_ENV_LOCK: tokio::sync::Mutex<()>` (a
  process-wide env-var test-serialization lock) and an `EnvRestore` RAII
  guard (`impl Drop`). Used by `annotations`, `auth`, `http`, `pipeline`
  (9 refs) — any split that separates these into different crates loses
  the shared lock unless `test_support` also becomes a shared
  `dev-dependencies`-only crate (or `#[cfg(test)]`-gated path dep), since
  today's single-binary env-var serialization guarantee is crate-global.
- **`tests/` integration dir** (crate root, not `src/`):
  `tests/spec_validation.rs`, `tests/registry_spec_validation.rs`, plus
  `tests/fixtures/` (68 files: `invalid/`, `invalid_registry/` YAML
  fixtures, `mirror-*.yml` fixtures, `pylock-pycowsay.toml`) and
  `tests/golden/` (15 golden-file `.txt` outputs for the CI-generation
  pipeline).
- **`CARGO_MANIFEST_DIR` sites**: 20+ call sites resolve fixtures as
  `concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/…")` or
  `/tests/golden` — spread across `spec/sign_config/tests.rs`,
  `spec/tests/env_sources.rs`, `spec/prescan/tests/mirror.rs`,
  `command/registry/sync/tests.rs`, `command/package/pipeline/plan/tests/
  pylock.rs`, `command/package/pipeline/generate/ci/tests/{support,
  leg_parity,golden}.rs`, `command/package/pipeline/prepare/tests/
  {pylock_env,subcommand}.rs`, `command/package/pipeline/push/tests/
  {push_driver,support,job_url,latest_alias,announce,env_push,retry}.rs`,
  `command/package/pipeline/announce.rs`, plus the two `tests/*.rs`
  integration files. **All resolve relative to the crate root's
  `CARGO_MANIFEST_DIR`** — a hard split hazard (§7).
- **Two non-test `CARGO_MANIFEST_DIR` sites** in `http.rs:379,428` (a
  production `#[cfg(test)]` self-scan, see §7) and `github_release.rs:20`
  (`concat!("ocx-mirror/", env!("CARGO_PKG_VERSION"))` — a `User-Agent`
  string; fine post-split, just needs the right crate's own version if
  `github_release` moves crates).
- **`include_str!`/`include_bytes!`** (§7 — cross-crate breakage risk):
  6 templates baked in via `include_str!("templates/*.yml")` /
  `include_str!("../templates/*.yml")` under
  `command/package/pipeline/generate/{ci.rs,ci/aux_workflows.rs}` — safe,
  stay within one future crate. 6 **self-scanning test** sites read
  sibling/cross-module `.rs` files as string data:
  `pipeline/registry_sync/tests.rs` (3×, one reaching into
  `command/package/mod.rs`), `pipeline/registry_sync/report/tests.rs` (2×),
  `pipeline/registry_copy/tests.rs`, `pipeline/push/tests.rs`,
  `command/package/pipeline/push/tests/cascade_backfill.rs`.
- **`build.rs`**: none.
- **Features**: `jsonschema = ["dep:schemars"]` (root `Cargo.toml`), gated
  in exactly two places — `command.rs:7,31,41` (the `schema` subcommand)
  and `spec/dist/tests.rs:382` (a test). Both are real production users
  of `schemars`, so `schemars` isn't just a `command`-owned optional dep;
  `spec` also needs it (non-optional in `spec`, per §4).
- **`cfg(feature = …)`**: only the 4 sites above.

---

## 7. Split hazards

Ranked by how much they constrain the target shape, not by file count.

1. **Five-way cycle cluster**: `command ↔ pipeline`, `spec ↔ pipeline`,
   `spec ↔ source`, `spec ↔ filter` (§2). These aren't accidental —
   `spec` types describe pipeline behavior (`registry_sync`, `dist_sync`
   destinations/globs/layouts) and pipeline/source read spec config back.
   Breaking every edge needs either a shared "spec+pipeline-config" crate
   or trait inversion; a naive 1:1 module→crate mapping will not compile.

2. **Whole-crate self-scanning tests read `src/` as text via
   `CARGO_MANIFEST_DIR`**: `http.rs:379,428` walks `src/` at test time to
   assert every OCI-client factory call site has a nearby
   `install_extra_roots()` seam, and hardcodes the path
   `src/command/package/pipeline/push.rs`. `pipeline/registry_sync/tests.rs:692`
   does `include_str!("../../command/package/mod.rs")` (compiles
   `command`'s source into `pipeline`'s test binary as a string literal).
   Both assume single-crate, single-`src/`-tree layout; they will not
   resolve once `http`/`pipeline` and `command` are separate crates with
   separate manifest dirs. These need to become a workspace-level xtask
   lint (or be deleted/rewritten as a `cargo metadata`-driven check) —
   not something `sed`-fixable per file.

3. **`tests/fixtures/` and `tests/golden/` are crate-root-relative**:
   20+ `CARGO_MANIFEST_DIR`-based test fixture paths in `spec`, `command`,
   `pipeline` test code all resolve against the *current* crate root.
   Splitting those three modules into separate crates means either (a)
   duplicating `tests/fixtures/` into each new crate, (b) a shared
   `dev-dependencies`-only fixtures crate, or (c) an env var/relative-path
   convention pointing back at a single retained fixtures directory. Pick
   one before moving any test file.

4. **`pub(crate)` items scale with module size and don't survive a crate
   boundary as-is**: `pipeline` 146, `spec` 30, `command` 22, `auth` 4,
   `source` 2 — total ~204 `pub(crate)` items. Every one used from a
   sibling top-level module (which the cycle list in §2 shows happens
   routinely — e.g. `spec/registry.rs` reaching into
   `pipeline::registry_sync::catalog`) needs its visibility widened to
   `pub` once that sibling is a separate crate; `pub(crate)` in the new
   crate would no longer be visible to the old sibling at all, so this
   isn't optional cleanup — it's a compile blocker discovered per-symbol
   during the actual split, needs a visibility audit pass first.

5. **`test_support.rs`'s `OCX_ENV_LOCK` static is a crate-global,
   process-wide env-var mutex** used by `annotations`, `auth`, `http`,
   `pipeline` tests (9 refs) to serialize `std::env::set_var` races across
   `cargo test`'s parallel threads. Splitting those four into separate
   crates loses the single global lock (`OnceLock`/`static` scope is
   per-compiled-artifact) — tests in different crates would no longer
   serialize against each other, reopening the exact race this exists to
   prevent, unless env-mutating tests are consolidated or forced
   single-threaded per crate.

6. **`filter` is not the "clean, spec-free, error-free" module it looks
   like from the `MirrorError` angle** — it depends on
   `crate::spec::{BackfillOrder, ResolvedBounds, VersionsConfig}` (real
   cycle, §2) and directly on `ocx_python::uv_pep440::Version`
   (`filter.rs:162-163,260`). A "generic filtering crate" extraction needs
   those spec types either passed in as generic parameters/traits or
   `filter` needs to stay downstream of `spec`.

7. **`jsonschema` feature spans two future crates**: `command.rs` (the
   `schema` subcommand) and `spec/dist/tests.rs` both gate on
   `feature = "jsonschema"`, and `schemars` is a non-test, non-optional
   dependency of `spec` per the actual `use` sites (§4) despite being
   `optional = true` only for the root crate's own conditional
   compilation. If `command` and `spec` split, the feature flag needs to
   live on whichever crate keeps the `schema` subcommand and be threaded
   through to `spec`'s (now non-optional, or its own re-exposed feature)
   `schemars` dependency.

8. **`error` (`MirrorError`) is depended on by 8 of 16 modules**
   (`command`, `spec`, `pipeline`, `source`, `junit`, `auth`, `discord`,
   `http` — §3) with 0 real outgoing edges of its own (the one
   `error→http` hit is a doc link, not code). This makes `error` the
   natural "leaf" shared crate every split piece depends on — the good
   news in this hazard list. No orphan-rule risk found: every `impl …
   for …` in the crate implements a foreign/std trait (`Display`, `Debug`,
   `Error`, `tower_service::Service`, `clap::ValueEnum`,
   `tokio::io::AsyncWrite`, `de::Visitor`) for a **locally-defined** type,
   which stays legal regardless of which crate ends up owning that type.

9. **Dependency surface doesn't shrink much for the two biggest
   modules**: `pipeline` touches 25 of ~31 external deps, `spec` touches
   18 (§4). Splitting them out does not meaningfully reduce their
   Cargo.toml — the payoff is dependency-graph *shape* (acyclic, testable
   in isolation) and parallel `cargo build`, not leaner manifests for
   those two crates specifically. The leaf modules (`resolver`,
   `run_summary`, `annotations`, `version_platform_map`, `normalizer`,
   `junit`) are where a minimal-Cargo.toml split pays off immediately.

10. **`ocx_python` (`crates/ocx_python`) is the one clean win already
    done**: zero `MirrorError`/`crate::spec` leakage confirmed by grep,
    self-contained `Cargo.toml`, its own `[lints.rust]` table, own
    `tests/fixtures/`. It is ready to promote to `ocx` as a published
    crate essentially as-is; the only coupling to the mirror is the
    inverse direction (`src/source/pylock.rs`, `spec/python_config.rs`,
    `spec/wheels.rs`, `pipeline/{python_prepare,python_push,lock_derive}.rs`
    calling into it), which is correct and expected.

11. No `macro_rules!` definitions anywhere in `src/` — no macro-hygiene
    concerns for the split.

12. Global mutable statics beyond the two testing ones above: a
    `pub static LATEST_TAGS_OVERRIDE: Mutex<Option<Vec<String>>>` in
    `command/package/pipeline/push/alias.rs:148` (test-injection hook for
    the `latest` alias logic) and an `EXTRA_ROOTS: OnceLock<ExtraRoots>`
    in `http.rs:52` (the one-time TLS root install gate `main.rs` calls
    before any client is built) — both are single-owner within their
    current module and don't cross a would-be crate boundary, low risk.
