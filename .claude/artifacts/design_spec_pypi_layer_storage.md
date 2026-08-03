# Design Spec: PyPI Wheel Layer-Storage (`pypi/<name>:sha256.<hash>`)

## Overview

**Status:** Draft
**Author:** Architect (`/architect`)
**Date:** 2026-07-18
**GitHub Issue:** N/A (branch `feat/pypi-mirror`)
**Related ADR:** [`adr_pypi_layer_storage.md`](./adr_pypi_layer_storage.md)
**Related Research:** [`research_pypi_layer_storage.md`](./research_pypi_layer_storage.md)

Formalizes ocx-mirror's ad-hoc wheel registration into first-class content-addressed layer-storage
repositories `<registry>/pypi/<package>:sha256.<hash>`, consumed exclusively via same-registry
`:from=` blob mount by env packages. Storages are raw OCI artifacts (empty-config, custom
`artifactType`), **not** OCX packages. This spec is the module touch-list, pipeline wiring, and test
strategy implementing ADR decisions Q1–Q6 within rulings R1–R5.

## Design Goals

- R1-literal storage manifests — no `Metadata`, no package semantics.
- D5-immune push path that survives the `external/ocx` bump onto main.
- Zero client-side change (R3): pull/materialize/GC stay provenance-blind.
- Minimal surface: no new pipeline phase, no `plan.json` schema bump, no GC subcommand.

## Component Contracts

### C1 — `wheel_reference` naming (crate `ocx_python`, `src/naming.rs`)

**Purpose:** Render the canonical repo-relative storage reference per R2.

**Change:**

```rust
// Fixed namespace constant replaces the configurable WheelScope + index-host segment.
pub const PYPI_NAMESPACE: &str = "pypi";

pub fn wheel_reference(wheel: &WheelRef) -> WheelReference {
    WheelReference {
        repository: format!("{PYPI_NAMESPACE}/{}", normalize_package_name(&wheel.name)),
        tag: format!("sha256.{}", wheel.sha256),   // canonical, period separator (Q1)
    }
}
```

**Behavior:**

| Input / Precondition | Expected Behavior | Postcondition |
|----------------------|-------------------|---------------|
| `WheelRef { name: "Flask-Cors", sha256: "abcd…" }` | repository `pypi/flask-cors`, tag `sha256.abcd…` | index-host + scope dropped |
| Two wheels, same package, different ABI | same `pypi/<name>` repo, distinct `sha256.<hash>` tags | dedup by content tag |
| `sha256` = 64 hex | tag = 71 chars, `[a-zA-Z0-9_][a-zA-Z0-9._-]{0,127}`-legal | parses via `Identifier` |

**Removals (unreleased — no back-compat):** `WheelScope`, `DEFAULT_WHEEL_SCOPE`, the
`extract_host`/`NO_URL_INDEX_HOST` machinery, and the `scope` parameter. `normalize_package_name`
stays (still reused by `compose`).

### C2 — `ocx_lib::push_artifact` (upstream `external/ocx`, additive — lands in the bump commit)

**Purpose:** R1-pure, D5-immune single-layer empty-config artifact push (ADR Q6). Third instance of
the empty-config + `artifactType` pattern already used by `push_description`/`push_patch_descriptor`.

**Public API:**

```rust
// Implemented on ocx_lib::oci::Client; Publisher forwards in one line
// (self.client.push_artifact(..)), the same shape as Publisher::push_description
// (publisher.rs:118). ocx-mirror holds a Publisher and calls that forward.
pub async fn push_artifact(
    &self,
    identifier: &oci::Identifier,        // registry + pypi/<name> + tag sha256.<hash>
    artifact_type: &str,                 // "application/vnd.ocx-mirror.wheel.v1"
    layer: LayerRef,                     // File(<repacked-tar path>)
    annotations: BTreeMap<String, String>,
) -> Result<oci::Digest>;               // manifest digest
```

`identifier` is `&oci::Identifier`, **not** `native::Reference` — the transport type is deliberately
non-public (`client.rs:154`) and every precedent (`push_description`, `push_patch_descriptor`) takes
`&Identifier`. The caller already builds this type: `wheel_tag_exists` constructs
`Identifier::new_registry(wheel_repository, registry)` (`python_push.rs:248`).

**Behavior:**

| Input / Precondition | Expected Behavior | Postcondition |
|----------------------|-------------------|---------------|
| valid `identifier` + tar layer | `push_blob` the empty-config `{}` blob + the layer, then build the manifest via `ManifestBuilder`: `artifactType`, `application/vnd.oci.empty.v1+json` config (`{}`), one `tar+zstd` layer, no `subject`, no platform; `push_manifest_raw` to the tag | `pypi/<name>:sha256.<hash>` manifest present |
| annotations non-empty | `org.opencontainers.image.title` = wheel filename on the layer descriptor | provenance readable |
| never | constructs `Metadata`/`Bundle`, touches `AuthoringMetadata`, `resolve_platform`, or `verify_dependency_pins` | R1 + D5 immunity |

**Escape hatch (fallback, not chosen):** if the upstream method is rejected, drive
`ocx_lib::oci::native::Client` (`push_blob` + `push_manifest_raw`) from ocx-mirror with
re-implemented auth (ADR Q6 Option 2).

### C3 — `register_wheel_layers` (crate `ocx_mirror`, `src/pipeline/python_push.rs`)

**Purpose:** Publish each not-yet-present wheel layer to its storage repo before the env push mounts
it (ADR Q3/Q5/Q6). Rewired from subprocess to a **direct** `push_artifact` call on the already-authed
`Publisher`/`Client`.

**Public API (signature evolves — takes the client, drops the registry-only string plumbing):**

```rust
pub(crate) async fn register_wheel_layers(
    publisher: &Publisher,      // already built for the fail-safe tag reads in this loop
    registry: &str,
    layers: &[EnvLayer],
    registered: &mut HashSet<String>,   // per-run dedup, keyed wheel_repository:wheel_sha256
);
```

**Behavior:**

| Input / Precondition | Expected Behavior | Postcondition |
|----------------------|-------------------|---------------|
| wheel already in `registered` set this run | skip (no round-trip) | in-process dedup |
| `wheel_tag_exists` → authoritative `true` | skip push | idempotent (Q5) |
| `wheel_tag_exists` → authoritative `false` | `push_artifact` the layer to `pypi/<name>:sha256.<hash>` | mount source present |
| `list_target_tags` transient error | **warn + skip** (never abort) | env push falls back to upload |
| `push_artifact` error | **warn + skip** | fallback upload (load-bearing) |

**Deletions:** `push_wheel_layer` (subprocess + `-p <arbitrary-key>` wart) and
`write_wheel_registration_metadata` (temp-file `Bundle` dance) — both replaced by the single
`push_artifact` call. The `platform: &str` parameter is dropped (storages are platform-free, Q2).

### C4 — `wheel_tag_exists` (same module)

**Purpose:** Fail-safe idempotency precheck (issue #157 semantics preserved).

**Change:** compare against the canonical tag, not the bare hex:

```rust
let canonical = format!("sha256.{wheel_sha256}");
Ok(tags.iter().any(|tag| *tag == canonical))
```

### C5 — `EnvLayer` / `build_env_push_args` mount tail (unchanged in shape)

The `:from=<wheel_repository>` tail (`build_env_push_args`, `python_push.rs:151-157`) uses the
**repository path only** — no tag. It tracks the new `pypi/<name>` path automatically once
`wheel_reference` changes; **no change needed** to the mount-tail construction itself. `EnvLayer`
fields (`wheel_repository`, `wheel_sha256`, `digest`, `path`, `package_name`) are unchanged.

## User Experience Scenarios

| # | User Action | Expected Outcome | Error Cases |
|---|-------------|------------------|-------------|
| 1 | `ocx-mirror package pipeline push` (pylock/pypi mirror) | each new wheel published once to `pypi/<name>:sha256.<hash>`; env push mounts it (`mounted > 0` in run-summary) | storage push fails → warn, env push uploads full layer (exit unaffected) |
| 2 | re-run push, wheels already stored | tag-exists skip; no re-push; env push mounts | transient tag-list error → warn, fallback upload |
| 3 | backfill-partial version | green legs replay; only genuinely-absent wheels re-registered | same fail-safe fallback |
| 4 | target registry unreachable at tag-list | fail-safe: registration skipped per wheel; run continues | authoritative not-found ≠ transient (issue #157) |

## Error Taxonomy

No new `MirrorError` variant. Storage registration is **best-effort upload-avoidance** — every
failure is `log::warn!` + skip, never propagated (matches current contract, `python_push.rs:196-236`).
Existing variants cover the surrounding flow:

| Failure Mode | Error Variant | Exit Code | Remediation |
|--------------|---------------|-----------|-------------|
| Storage tag-list transient error | (swallowed — warn only) | n/a | env push falls back to upload |
| Storage push failure | (swallowed — warn only) | n/a | env push falls back to upload |
| Env push itself fails | `MirrorError::ExecutionFailed` (via existing push path) | 1 | inspect leg logs |
| Target read genuinely unavailable (surrounding `plan`/`sync` path) | `MirrorError::TargetError` | 69 | retry (fail-safe, #157) |

Rationale (quality-core "no silent swallowing"): each swallow is logged with context and documented
as intentional upload-avoidance — not a silent `.ok()`.

## Edge Cases

- **Repack determinism (load-bearing).** Idempotency keys on the `.whl` hash (tag); mount keys on the
  repacked-tar digest. Non-deterministic repack ⇒ tag exists but mount misses ⇒ silent fallback to
  upload. Depends on `ocx_python::repack_wheel` producing a stable tar digest — see Testing Strategy.
- **Same wheel across platforms/versions in one run** — `registered` HashSet dedups; only the first
  leg round-trips.
- **URL-less wheel** — `select` already rejects these upstream (`naming.rs` doc); with the index-host
  segment gone, the `NO_URL_INDEX_HOST` fallback is deleted entirely.
- **Non-parseable PEP 440 app version** — orthogonal; affects env-package cascade, not storage (which
  is un-cascaded, single flat tag).
- **Empty-config sentinel rejected by an exotic registry** — surfaced by acceptance test; `:5000`
  distribution accepts it; documented raw-native fallback exists.

## Trade-off Analysis (push mechanism — the pivotal choice)

| Criterion (weight) | A: direct `ocx_lib::push_artifact` | B: subprocess `ocx package push` (+D5 shim) |
|--------------------|-----------------------------------|---------------------------------------------|
| R1 fidelity (3) | full — no `Metadata` | violates — fabricated package |
| D5 immunity (3) | full | fights D5 permanently |
| Reuses existing authed client (3) | yes (Publisher already in loop) | yes (ocx binary) |
| Removes temp-file + `-p` wart (2) | yes | no |
| New surface (2) | one additive upstream method | none but permanent shim tax |
| Reversibility (2) | two-way (additive) | one-way (shim rot) |

**Reversibility:** Two-Way Door for the API shape; One-Way Door Medium for published tags/manifests
(content-addressed, so re-derivable but persistent).
**Recommendation:** Option A — direct `push_artifact`. R1-pure, D5-immune, deletes two warts, reuses
the client already constructed for the fail-safe reads. (ADR Q6.)

## Module Placement

| Change | Location |
|--------|----------|
| `wheel_reference` → `pypi/<name>:sha256.<hash>`; delete `WheelScope`/index-host | `crates/ocx_python/src/naming.rs` |
| Drop `WheelScope` re-export | `crates/ocx_python/src/lib.rs` (exports) |
| **New** `push_artifact` (empty-config artifact push) — on `Client`, plus a one-line `Publisher` forward | `external/ocx/crates/ocx_lib/src/oci/client.rs` + `publisher.rs` — lands in the bump commit |
| `register_wheel_layers` → direct `push_artifact`; drop `platform`, delete `push_wheel_layer` + `write_wheel_registration_metadata` | `crates/ocx_mirror/src/pipeline/python_push.rs` |
| `wheel_tag_exists` → compare canonical `sha256.<hash>` tag | `crates/ocx_mirror/src/pipeline/python_push.rs` |
| `register_wheel_layers` call site: drop `platform_str` arg | `crates/ocx_mirror/src/command/package/pipeline/push.rs:489-496` |
| **Remove** `wheel_scope` spec field + `default_wheel_scope` + its validation/tests | `crates/ocx_mirror/src/spec.rs` (fields ~63-67, 173-175; tests ~1154-1195) |
| Bump compile fix: drop `os_version` from destructure | `crates/ocx_mirror/src/spec/wheels.rs:69-84` |
| Bump compile fix: drop `os_version`/`features` from `Platform::Specific` | `crates/ocx_python/src/compose.rs:526-533` |
| `SelectedWheel.wheel_repository` / `WheelEnvTask.wheel_scope` — drop `wheel_scope` threading | `crates/ocx_mirror/src/pipeline/python_prepare.rs` + `command/package/pipeline/prepare.rs:324,369` |
| Acceptance test: assertion `sha256 in tags` → `f"sha256.{sha256}" in tags`; docstring `pip-packages/...` → `pypi/<name>:sha256.<hash>` | `test/tests/test_mirror_mount.py:243` + docstring (lines 7-11) |

**Explicitly NOT touched (YAGNI / ADR Q3–Q4):** `plan.rs` (`PlanReport`/`PlanAssetEntry` schema —
no wheel-hash field added), `version_platform_map.rs`, any generated workflow template, any GC/task
runner, the `orchestrator.rs` phase model. No new `MirrorError` variant.

## Pipeline Phase Wiring

Unchanged phase model (subsystem-mirror Phase 2, sequential by version). Storage registration stays
an **inline helper** inside the serial push loop, called per green `(V,P)` leg immediately before
`invoke_env_push` — guaranteeing the storage manifest is present before the mount (ADR Q3). No new
phase, no new CI job, no `pipeline generate ci` template change.

```
Phase 2 (push, serial, oldest first)
  for each version:
    for each green (V,P) leg:
      register_wheel_layers(publisher, registry, layers, &mut registered)   # push_artifact per absent wheel  ← storage FIRST
      invoke_env_push(... layers each carrying :from=pypi/<name> ...)        # mounts the blob                  ← consume SECOND
```

`plan.json` / backfill impact: **none.** Storage publication is derived per run from the fail-safe
tag-exists check, not tracked in `plan.json` or `VersionPlatformMap` (ADR Q3). `BackfillPartial`
legs replay and re-check each wheel; content addressing makes re-registration byte-identical.

## Migration of Current App-Repo Wheel Registration

Feature is unreleased on `feat/pypi-mirror` → straight replacement, no shim:
1. `wheel_reference`: `pip-packages/<host>/<name>:<bare-hex>` → `pypi/<name>:sha256.<hash>`.
2. Push mechanism: `ocx package push -m <Bundle>` subprocess → direct `push_artifact` (empty-config).
3. Idempotency check: compare canonical `sha256.<hash>` tag.
4. Remove `wheel_scope` spec knob (R2 fixes the namespace to `pypi`).
5. Land the two `Platform::Specific` compile fixes in the same submodule-bump commit as the
   `publisher.rs` reconcile (ADR Upstream Alignment).

## Testing Strategy

| Level | What | Where |
|-------|------|-------|
| Unit | `wheel_reference` renders `pypi/<name>:sha256.<hash>`; drops index-host/scope; two ABIs → one repo, two tags | `crates/ocx_python/src/naming.rs` `#[cfg(test)]` |
| Unit | `wheel_tag_exists` matches canonical `sha256.<hash>`, not bare hex | `crates/ocx_mirror/src/pipeline/python_push.rs` `#[cfg(test)]` |
| Unit | `push_artifact` builds empty-config manifest: `artifactType` set, `application/vnd.oci.empty.v1+json` config, one layer, no `subject`, no platform | `external/ocx/crates/ocx_lib/src/oci/client.rs` `#[cfg(test)]` |
| Unit | repack determinism — same wheel bytes → same tar digest (the Q5 invariant) | `crates/ocx_python` repack tests |
| Unit | `build_env_push_args` still emits `:from=pypi/<name>` mount tails in wheel order | existing test at `python_push.rs:346-416`, update expected repo strings |
| Acceptance | env push publishes `pypi/<name>:sha256.<hash>` then mounts it; run-summary reports `mounted > 0` (mount-hit path) | **`test/tests/test_mirror_mount.py`** (already exists — update in place, do not create a new file) against `:5000` |
| Acceptance | idempotent re-run: tag-exists skip, no re-push, still mounts | `test/tests/test_mirror_mount.py` (extend) against `:5000` |
| Acceptance | storage-push failure path: env push falls back to full upload, run still green | `test/tests/` (inject unreachable/denied storage) |

**`test_mirror_mount.py` is the pre-existing home for this feature and hardcodes the pre-ADR tag
grammar** — updating it is part of C1/C4, not new-test authoring:
- **`test_mirror_mount.py:243`** — `assert sha256 in tags` (bare hex) must become
  `assert f"sha256.{sha256}" in tags` once the canonical `sha256.<hash>` tag lands (C1/C4).
- **Docstring (lines 7-11)** — references the pre-ADR `pip-packages/...:<sha256>` naming; update to
  `pypi/<name>:sha256.<hash>` (empty-config artifact, `:from=` mount source) alongside the code change.

Acceptance harness: session-scoped `registry:2` on `localhost:5000` (`test/conftest.py`), disposable
per session — no GC teardown needed (ADR Q4). Single test:
`cd test && uv run pytest tests/test_mirror.py::<name> -v --no-build`.

## Documentation Impact

| Surface | File | Change |
|---------|------|--------|
| mirror.yml reference | `docs/…` | remove `wheel_scope` field; document fixed `pypi/<name>` storage namespace |
| Storage-namespace concept | `docs/…` | new short section: content-addressed `pypi/*`, `sha256.<hash>` tags, `:from=` mount, GC-safety + Harbor/zot retention operator note (ADR Q4) |
| subsystem-mirror rule | `.claude/rules/subsystem-mirror.md` | update the `python_push.rs` row (`pip-packages/...` → `pypi/<name>:sha256.<hash>`, empty-config artifact, direct `push_artifact`) |
| Changelog | repo changelog | `feat(mirror): pypi wheel layer-storage repos` |

---

## Approval

| Role | Name | Date | Status |
|------|------|------|--------|
| Engineering | | | Pending |
