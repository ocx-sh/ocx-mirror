# Design Spec: `ocx_python` crate

<!--
Component design spec. Owner: /architect. Companion to
adr_ocx_python_crate.md (workspace/boundary decision). API sketches are
contracts, not implementation.
-->

## Metadata

**Status:** Proposed (rev 2 — platform mapping, selection, testing added)
**Date:** 2026-07-04
**Related ADR:** [adr_ocx_python_crate.md](./adr_ocx_python_crate.md)
**Research:** [research_python_wheel_oci.md](./research_python_wheel_oci.md)

## Purpose

Pure translation library: PEP 751 `pylock.toml` in → OCX package
compositions out. Encodes the cross-repo conventions (wheel naming, repack
determinism, layer layout, entrypoint synthesis, platform/axis encoding)
exactly once, for ocx-mirror today and ocx-dist later. No registry I/O,
no HTTP.

## Scope

| In scope | Out of scope (consumer-owned) |
|---|---|
| pylock parse + validation (wheels-only, hash-required) | uv.lock parsing (require pylock export) |
| Marker evaluation + wheel selection per target | Wheel download (mirror's `pipeline/download.rs`) |
| Platform/axis mapping (wheel tags → OCX platform + variant) | Registry existence checks / diffing |
| Conventional wheel repo naming (pure function) | Push, cascade tags, run summaries |
| Deterministic wheel→tar.zst repack + `.data` relocation | CI generation, notify, spec YAML (`MirrorSpec` extension stays in the mirror) |
| Env-package composition: layer descriptors + prefix layout, entrypoint synthesis, interpreter dep, env metadata | sdist building (wheels-only, hard error) |
| Publish-time collision pre-check across wheel path sets | Lock *generation* for published apps (`uv pip compile` wrapper — mirror-side, v2) |

## Platform & axis model

A Python target is 5-axis: `(os, arch, libc{family,floor}, python, abi)`.
OCX platform (os/arch enums + index entries) carries 2. Mapping is layered:

```
L1  wheel tag → facts       PEP 425/600/656 parse; FROZEN in code
    manylinux_2_28_x86_64 → {os:linux, arch:amd64, libc:gnu, libc_min:2.28}
    musllinux_1_2_aarch64 → {os:linux, arch:arm64, libc:musl, libc_min:1.2}
    macosx_11_0_arm64     → {os:darwin, arch:arm64, os_min:11.0}
    win_amd64             → {os:windows, arch:amd64}
    any / py2.py3 / abi3  → resolved via tag-compat semantics, not equality
L2  facts → OCX encoding    grammar-versioned in code (NOT user config)
    v1 (today): os/arch → platform key + index entry; libc/abi → mirror
        variant tag prefix (default = gnu+primary ABI; `musl-`, `cp313t-`)
    v2 (planned, upstream `+libc.gnu` platform-grammar MR): libc moves into
        the platform key / index entry; requires widening the mirror's
        PLATFORM_KEY_RE (2-segment regex today) — align with the MR before
        either lands
L3  spec = target selection user-facing configuration surface
    platform keys select os/arch; variants compose L1-fact constraints
```

Invariant: L1/L2 identical across all namespace writers (ocx-mirror,
ocx-dist) — that is what the crate + conventions ADR protect. L3 is free.

Variant constraint vocabulary (bounded to L1 fact fields — no free-form
tag regex):

```yaml
variants:
  default: { libc: gnu, min_manylinux: "2_28" }    # unadorned tag chain
  musl:    { libc: musl, min_musllinux: "1_2" }    # musl-<ver>_<TS> chain
  cp313t:  { abi: cp313t }                         # free-threaded ABI
```

Both libc families are dynamic-link families with versioned floors
(PEP 600: glibc ≥ X.Y; PEP 656: musl ≥ X.Y — musllinux is NOT static
musl). Floors are per-family (`min_manylinux` / `min_musllinux`).
Asymmetry note: uv's `--python-platform` offers explicit manylinux floors
but only a floorless `*-unknown-linux-musl` value — v2 lock derivation for
musl inherits uv's default floor; our own `select` honors `min_musllinux`
exactly. The musl env additionally requires the musl-linked
python-build-standalone interpreter variant (dynamic on both sides).

Each variant × platform key = one env composition = one selection run.
Canonical variant names (`musl`, `cp313t`, …) reserved in the conventions
ADR so consumers don't publish `glibc` vs `gnu` for the same axis.

## Wheel selection algorithm

Per (variant, platform key):

1. **Marker environment** derived from (L1 facts, interpreter pin):
   `python_version`, `python_full_version` (from interpreter package),
   `sys_platform` (linux/darwin/win32), `platform_machine`
   (x86_64/aarch64/arm64/AMD64), `platform_system` (Linux/Darwin/Windows),
   `os_name` (posix/nt), `implementation_name`/`platform_python_implementation`
   (cpython/CPython). Table versioned with the convention.
2. **Package filter**: evaluate each lock entry's `marker` (and
   `environments` intersection) via `uv-pep508` + `uv-pep440`. Covers
   OS-forks (`colorama; sys_platform=="win32"`), inverted forks
   (`watchdog; platform_system != "Darwin"`), implementation forks
   (`brotli` vs `brotlicffi`).
3. **Target tag set**: ordered priority list from `uv-platform-tags` for
   (python, abi, platform facts), constrained by the variant
   (`min_manylinux` floor, libc family). Handles `abi3` (spans minors),
   `py2.py3-none-any` (union tags), `any` — compat semantics, never string
   equality.
4. **Candidate pick**: parse each wheel filename
   (`uv-distribution-filename`), keep those intersecting the target set,
   rank by tag priority; tiebreak by build tag (PEP 427), then filename
   (deterministic). Zero candidates → `SelectError` naming package, triple,
   variant, and the tags that WERE available (actionable: psycopg2-style
   "no Linux wheel" vs uwsgi-style "no wheel anywhere" distinguished).
5. **Set validation**: all binary wheels ABI-consistent with the
   interpreter pin (`cp313` vs `cp313t` fails closed); extras accounting
   for entrypoint synthesis (below).

## Module map

| Module | Responsibility | Key deps |
|---|---|---|
| `lock` | PEP 751 subset parser; reject sdist-only entries, missing hashes | `toml`, `serde` |
| `platform` | L1 fact parse + L2 encoding (grammar-versioned); marker-env derivation | `uv-platform-tags` |
| `select` | Algorithm above | `uv-distribution-filename`, `uv-pep508`, `uv-pep440` |
| `naming` | Conventional repo path + tag — THE naming convention encoding | none |
| `repack` | wheel zip → deterministic tar.zst; `.data/scripts`→`bin/`, `.data/data`→content root (`share/…`), purelib/platlib→`lib/site-packages` | `zip` read, tar+zstd write |
| `compose` | env-package composition: per-layer prefix layout, entrypoints, interpreter dep, env metadata | `ocx_lib` types |
| `collide` | Pre-publish path-set collision check across selected wheels | none |
| `error` | `thiserror`, `#[non_exhaustive]`, `#[source]` chains | `thiserror` |

Pinned uv crates (git rev, workspace-level, bump procedure documented next
to submodule procedure): `uv-distribution-filename`, `uv-platform-tags`,
`uv-pep508`, `uv-pep440`.

## API contract (sketch)

```rust
pub fn parse_pylock(input: &str) -> Result<Pylock, LockError>;

pub fn select_wheels(lock: &Pylock, target: &PythonTarget)
    -> Result<Vec<WheelRef>, SelectError>;
// PythonTarget = { platform_key, variant_constraints, interpreter }
// WheelRef carries name, version, filename, url, sha256

pub fn wheel_reference(scope: &WheelScope, wheel: &WheelRef) -> WheelReference;
// renders "<scope>/<index-host>/<package>/<slug>:<sha256>"

pub async fn repack_wheel(wheel_path: &Path, output_dir: &Path)
    -> Result<RepackedWheel, RepackError>;
// RepackedWheel { layer_path, layer_digest, wheel_sha256, entry_points,
//                 record_paths, locked_extras }

pub fn check_collisions(wheels: &[RepackedWheel]) -> Result<(), CollisionError>;

pub fn compose_env(spec: &EnvSpec, wheels: &[RepackedWheel])
    -> Result<EnvComposition, ComposeError>;
// EnvComposition {                               // TARGET-AGNOSTIC — no registry host
//   metadata: ocx_lib::package::metadata::Metadata, // Bundle: entrypoints
//               // (command=python3, args=["-c",shim]), env (PYTHONPATH,
//               // PATH, PYTHONDONTWRITEBYTECODE=1), interpreter dependency
//   platform: ocx_lib::oci::Platform,          // L2 encoding
//   layers: Vec<WheelLayer>,                    // source + LayerLayoutSpec (empty)
// }
// EnvComposition::into_info(self, id: oci::Identifier) -> Info
//   — the SOLE seam where the consumer injects the registry host. A full Info
//   requires an Identifier (registry-bearing), which this crate never knows;
//   compose emits the two registry-independent thirds (metadata + platform).
```

### Entrypoint synthesis rules

- `[console_scripts]` object references use the FULL grammar
  `module[:attr[.attr…]]` (dotted attribute chains; module-only refs
  valid) → entrypoint `name`, `command: python3`, `args: ["-c", <shim>]`
  where the shim resolves via `importlib.import_module(module)` + a
  `getattr` walk over the attr chain — never a literal
  `from {mod} import {func}` template (breaks on dotted attrs).
  Argv array, no shell.
- **Extras-gated scripts** (`blackd = blackd:main [d]`): synthesized only
  when the extra is requested — decision input is
  `EnvSpec.requested_extras` (consumer-declared, e.g. `app[full]`)
  validated against the lock's top-level `extras` key; never inferred
  from dependency presence. A registered-but-unresolvable launcher fails
  at first run otherwise.
- `python3` resolves via the private interpreter dependency on the composed
  PATH; ABI mismatch fails at compose, not at run.

### Runtime-write mitigation (read-only hardlink CAS)

- `PYTHONDONTWRITEBYTECODE=1` in env metadata (v1) — CPython otherwise
  writes `__pycache__/` into package content on first import.
- Pre-baked hash-based `.pyc` (PEP 552 unchecked-hash) at repack = v2
  startup optimization; must stay deterministic.
- Known limitation (documented, not solved): packages JIT-writing next to
  source (`numba`) need their cache env vars redirected; smoke tests catch.

## Mirror integration (config sharing)

`source.type: pylock` slots into `MirrorSpec` — explorer-verified as a
data-layer change only:

1. `Source::Pylock { … }` variant in `spec/source.rs`.
2. `src/source/pylock.rs` returning `Vec<VersionInfo>` (version = project
   version from the lock).
3. One match arm in `sync.rs::list_upstream_versions`.
4. **Zero renderer/template changes** — generated workflow is
   source-agnostic (plan.json-driven, discover→prepare→test→push→notify).

Shared spec surface (unchanged semantics): `target`, `platforms` (runner
map), `variants`, `cascade`, `build_timestamp` (normalizer `_<TS>`),
`notify`, `catalog`, `tests`. Python-specific: `python:` block
(interpreter version/ABI → python-build-standalone dep), `wheel_scope`,
variant constraint fields, per-platform `min_manylinux`.

**Known friction**: `VersionInfo.assets = HashMap<platform, Url>` — one
asset per platform; a wheel set is N per platform. Options (decide in
plan): (a) widen assets to `Vec<Url>` in plan schema (v2 already carries
resolved per-entry assets), (b) lock-as-single-asset + ocx_python-driven
wheel fetch inside prepare. Option (a) preferred — keeps download,
resumption, and concurrency in the existing pipeline.

**v2 — published apps without a lock** (`source.type: pypi`): wheels ship
no lock (verified — F9); derive one per target via
`uv pip compile - --format pylock.toml --python-platform … --exclude-newer <stamp>`
(uv as build-time tool; reproducible via upload-time cutoff), persist the
generated pylock in the mirror repo. Not in v1.

Lock-derivation platform mapping (verified against `uv pip compile
--python-platform` possible values, uv 0.11.19): variant constraints render
to **explicit** values — `{libc:gnu, min_manylinux:2_28, arch:amd64}` →
`x86_64-manylinux_2_28`; `{libc:musl}` → `x86_64-unknown-linux-musl`;
darwin/windows → bare triples. Never emit bare `x86_64-unknown-linux-gnu`
(that delegates the glibc floor to uv's default assumption instead of the
spec's). `--python-version` comes from the spec's `python:` block. Same
fixed-table discipline as L2: mapping lives in code, floor policy in spec.

**Toolchain bootstrap**: the lock-derivation step (and acceptance CI) gets
uv — and the pinned interpreter — from OCX itself: uv is already published
as an OCX package; the python-build-standalone mirror spec (open question
3) provides the interpreter package. Generated workflows already bootstrap
via setup-ocx + `ocx run`; the python feature adds pinned uv/python deps to
that toolchain rather than a second install mechanism. Verify at plan time
whether `uv pip compile` with explicit `--python-version`/`--python-platform`
resolves without a host interpreter — moot operationally, since the
OCX-bootstrapped interpreter is present either way.

### Mirror vs ocx-dist boundary

| | ocx-mirror | ocx-dist (future) |
|---|---|---|
| Input | published upstream artifacts (PyPI app at pinned version; derived lock) | the project's OWN pylock, from its repo/CI |
| Trigger | discovery: watch upstream, version filter, drift (`--exclude-newer` stamp bumps) | project release: push-based, no discovery |
| Metadata | mirror-owned catalog/annotations per version×platform (as with `github_release` today) | project-owned metadata |
| Python source type | `source.type: pypi` (v2) | native lock intake |

v1 `source.type: pylock` in the mirror is a pragmatic stand-in for the
dist flow (the mirror has the pipeline today); when ocx-dist lands, the
first-party-lock flow migrates there and the mirror keeps `pypi`. Both
sides call the same `ocx_python` — which is the point of the crate split.

## Testing strategy

### Corpus (acceptance, tiered — research F8)

| Tier | App | Gate |
|---|---|---|
| easy | `pycowsay` | every acceptance run (seconds; zero-dep baseline) |
| medium | `yt-dlp`, `black` | default acceptance suite (markers, mypyc cpXY wheels, extras-gated script) |
| hard | `streamlit`, `google-cloud-aiplatform[full]` | opt-in/nightly marker (heavy stack, PEP 420 union at scale, 150+ layers) |
| negative | `uwsgi`, `psycopg2` | default suite — assert actionable failure messages (no-wheel-anywhere vs no-wheel-for-triple) |

Unit fixtures (tiny handcrafted wheels in `crates/ocx_python/tests/fixtures/`):
pure, cpXY C-ext stub, abi3, PEP 420 namespace pair, `.data/{scripts,data}`,
console_scripts incl. extras-gated, legacy `nspkg.pth`, `py2.py3-none-any`.
Golden repack test (fixed wheel → byte-identical tar.zst digest) locks
determinism, analogous to ocx's manifest byte-golden test.

### Platform legs & limitations (explicit)

- Runner matrix per `platforms:` keys + starlark `ocx package test`
  (`ocx.run("app", …)`, `ocx.target_platform`) exercises the composed env
  per os/arch — entrypoint dispatch, private interpreter, C-ext ABI.
- **Container legs are rejected today** (`policy_check_no_containers`,
  exit 64; capability deferred "Phase 8") — **revival is authorized and
  planned as part of this feature's finalization stage**: re-enable
  container test legs in the ocx-mirror test environment with an alpine
  image (musl variant validation) and a debian image (glibc-floor sanity
  on an older glibc than ubuntu-latest). Until that lands:
  - `musl` variant untestable in CI (deferred to the container-leg PR).
  - **glibc-floor gap**: ubuntu-latest glibc > 2.28; floor violations pass
    natively. Wheel platform tag treated as upstream's compatibility
    promise — smoke-test import/run only. Closed by the debian leg.
- Free-threaded (`cp313t`) variant: wheels exist for numpy/pandas/pyarrow
  but coverage uneven — keep as unit-fixture axis, not CI leg (2026
  guidance: test, don't default-deploy).

## Error model

| ocx_python error | MirrorError wrap | Exit |
|---|---|---|
| `LockError` (parse, sdist-only, missing hash) | new variant | 65 DataError |
| `SelectError` (no compatible wheel; names package+target+available tags) | new variant | 65 DataError |
| `RepackError` (io/zip) | `ExecutionFailed` | 1 |
| `CollisionError` | new variant (fail before push) | 65 DataError |
| `ComposeError` (ABI mismatch, bad entry point, unknown extra) | new variant | 65 DataError |

## Conventions (one-way-doors — upstream ADR before first publish)

1. **Naming**: `<scope>/<index-host>/<package>/<slug>:<sha256>`; scope
   maintainer-configured (default `pip-packages`); slug for build/variant
   disambiguation.
2. **Repack determinism**: sorted entries, epoch mtimes, uid/gid 0, mode
   normalization, pinned zstd level; versioned `repack-v1` annotation.
3. **Layout**: `repack` emits the FINAL relocated tree per wheel —
   purelib/platlib→`lib/site-packages/`, `.data/scripts`→`bin/`,
   `.data/data`→content root (`share/…`) — so each wheel is ONE
   content-addressed layer applied at the content root with an EMPTY
   `LayerLayoutSpec` (a wheel spans three destination prefixes, which a
   single layer prefix cannot express). Env metadata: `PYTHONPATH`, `PATH`,
   `PYTHONDONTWRITEBYTECODE=1`.
4. **Entrypoint synthesis** incl. extras gating (above).
5. **Platform/axis encoding**: L1 fact table + L2 v1 (variant prefixes,
   canonical names) and v2 (`+libc` grammar) migration note — L1 facts
   stable across both; v1→v2 = republish.
6. **Marker-environment derivation table** (versioned).

## Implementation environment & rollout (MANDATORY constraints)

- **Registry**: all implementation-phase publishing goes to **dev.ocx.sh**
  — never the release registry.
- **Test repository**: create **`ocx-contrib/mirror-pypi`** as the live
  integration repo (spec + generated workflows + pylock fixtures). Org
  repos already carry the required environment variables:
  `OCX_MIRROR_DISCORD_HOOK` (→ `notify.discord.webhook_secret` name),
  `OCX_MIRROR_REGISTRY_TOKEN`, `OCX_MIRROR_REGISTRY_USER` (registry auth)
  — no per-repo secret setup.
- **Fast iteration loop**: publish work-in-progress ocx-mirror via this
  repo's **`Deploy Dev`** workflow → `dev.ocx.sh/ocx/mirror:<ver>-dev_<TS>`;
  `mirror-pypi` pins that dev build in its **`ocx.toml`** so the identical
  binary bootstraps locally (direnv export) and in CI (setup-ocx) — one
  pinned toolchain, no second install path.
- **Platform rollout**: start **linux/amd64 only** (spec `platforms:` has
  one key); add darwin/arm64, linux/arm64, … in the finalization stage.
- **Container-leg revival** (finalization): re-enable docker test support
  in the ocx-mirror test environment — alpine leg (musl) + debian leg
  (older-glibc floor sanity). Lifts `policy_check_no_containers`; scope
  per the Phase 8 note in `generate/ci.rs`.

## Open questions (for /swarm-plan)

1. Wheel-blob placement: cross-repo OCI blob mount vs re-push per env repo
   — `Publisher` capability check; storage-dedup only.
   **RESOLVED (W2.4, 2026-07-04): re-push per env repo.** `Publisher` has
   NO cross-repo blob-mount capability (`LayerRef::Digest` HEAD-verifies a
   blob only in the manifest's OWN repo; blobs always upload to the manifest
   repo — `publisher/layer_ref.rs`, `oci/client.rs:729,804`). Option A
   (cross-mount) is therefore not implementable now. Env push uses
   `LayerRef::File{layer_path, LayerLayoutSpec::default()}` per wheel →
   the wheel blob is re-uploaded into the env repo. The wheel's own
   content-addressed repo (`<scope>/<host>/<pkg>/<slug>:<sha256>`) is still
   published (upload-if-missing) as the discoverable "layer repo" the goal
   requires, but it is a SEPARATE registration — the env package is
   self-contained (all layers present in the env repo), so `ocx run` /
   `ocx package test` never depend on the wheel repos. Cross-mount is a v2
   storage optimization, gated on a future `Publisher` mount API.
2. `VersionInfo` multi-asset extension — option (a) vs (b) above.
3. Interpreter package pipeline: mirror python-build-standalone via
   existing `github_release` source type — sequencing vs PR 3.
4. `+libc` platform-grammar MR alignment (PLATFORM_KEY_RE widening,
   index-entry vs variant placement) — coordinate before either lands.
