# ADR: `ocx_python` upstream-packaging conventions

<!--
Documents the SET of already-implemented, cross-repo conventions the
`ocx_python` pure-translation crate encodes. One coherent decision:
"adopt these deterministic, target-agnostic conventions". The bulk lives in
Technical Details, one subsection per convention. Companion to
adr_ocx_python_crate.md (workspace/boundary decision).
-->

## Metadata

**Status:** Accepted
**Date:** 2026-07-04
**Deciders:** pylock-mirror swarm
**GitHub Issue:** N/A (W4 deliverable)
**Related Design Spec:** [design_spec_ocx_python.md](./design_spec_ocx_python.md)
**Stack Alignment:**
- [x] Decision fits existing stack (Rust 2024 + Tokio, see CLAUDE.md) and conventions in `.claude/rules/subsystem-mirror.md`
**Domain Tags:** spec | docs
**Supersedes:** N/A
**Superseded By:** N/A

## Context

`ocx_python` (`crates/ocx_python`) is a pure translation library: PEP 751
`pylock.toml` in → OCX package compositions out. It performs no registry I/O and
stays target-agnostic (`lib.rs` "Boundary"). Every rule it applies when turning a
locked Python app into an OCX env package is a **one-way door**: two writers of the
shared registry namespace (`ocx-mirror` today, `ocx-dist` later) must produce
byte-compatible artifacts, or the namespace forks irreversibly.

The crate is implemented. This ADR is the durable reference that pins the
conventions its modules already encode — `lock`, `platform`, `select`, `naming`,
`repack`, `compose`, `collide` (module map in
[subsystem-mirror.md](../rules/subsystem-mirror.md) and the design spec). It
consolidates the design spec's "Conventions" section against the shipped code and
adds the collision convention the spec left implicit.

## Decision Drivers

- **Convention integrity** — one code implementation of each one-way-door rule,
  consumed by all namespace writers; drift corrupts a shared registry.
- **Determinism / content-addressability** — identical inputs must yield the
  identical layer digest, so blobs dedup and re-runs are no-ops.
- **Target-agnosticism** — the crate must not embed a registry host; the host is
  a single, explicit consumer seam.
- **Fail-closed safety** — malformed lock content, ABI drift, path overlap, and
  zip-bombs abort before anything is pushed, not at the user's runtime.

## Considered Options

### Option 1: Deterministic, target-agnostic conventions encoded once (chosen)

Every rule frozen in code, grammar-versioned where it can evolve
(`REPACK_VERSION`, `L2_GRAMMAR_VERSION`), emitting repo-relative identifiers and
the two registry-independent thirds of an `Info` (`Metadata` + `Platform`).

| Pros | Cons |
|------|------|
| One source of truth for cross-repo artifacts | Convention changes are republish events |
| Byte-identical, content-addressed layers | Grammar-version bookkeeping needed |
| Host injected at exactly one seam | Crate cannot short-circuit registry checks |

### Option 2: Host-coupled composition (rejected)

Let the crate assemble a full `Info` with the registry `Identifier` baked in.
Rejected: couples the translation logic to the mirror's registry, blocks the
ocx-dist reuse that motivates the crate, and defeats content-addressing.

### Option 3: Per-writer ad-hoc conventions (rejected)

Each consumer implements naming/repack/layout itself. Rejected: guarantees drift
in the one place drift is unrecoverable — the shared namespace.

## Decision Outcome

**Chosen Option:** Option 1. The crate encodes the seven conventions below.
Each is grounded in the module/constant/function that implements it; the crate's
public error types map to consumer exit codes (`error.rs`, and
[subsystem-mirror.md](../rules/subsystem-mirror.md) "Error Model").

## Technical Details

### Convention 1 — Naming (PEP 503 normalization + conventional repo path)

`naming::wheel_reference(scope, wheel)` renders the repo-relative, host-free
reference `<scope>/<index-host>/<package>:<sha256>`:

- **Package segment** — `naming::normalize_package_name` (`naming.rs`): lowercase,
  and collapse runs of `-` / `_` / `.` to a single `-` (equivalent to
  `re.sub(r"[-_.]+", "-", name).lower()`). `Flask_Cors` → `flask-cors`,
  `A.B_C-D` → `a-b-c-d`.
- **Scope** — `WheelScope`, defaulting to `DEFAULT_WHEEL_SCOPE = "pip-packages"`;
  maintainer-configurable.
- **Index-host** — the URL authority via a hand-rolled `extract_host` (no `url`
  dep in this crate); folds `.`/`..` to the `unknown-index-host` fallback as a
  CWE-22 path-traversal guard.
- **Tag** — the wheel's `sha256` (hex, no `sha256:` prefix): content-addressed,
  and therefore the *sole* disambiguator. Wheels of one package that differ by
  build tag / ABI / platform share the `<package>` repo as distinct tags;
  byte-identical wheels (e.g. an `abi3` wheel shared across CPython minors)
  dedupe onto one tag. An earlier revision carried an extra `<slug>` path
  segment (ABI+platform tags) for this — dropped as redundant with the content
  hash, which already distinguishes every distinct wheel.

The reference carries **no registry host**; the consumer prepends the registry
when it builds the final `ocx_lib::oci::Identifier`.

**Mirror-side mirror of the normalizer.** The app-name match in the mirror's
`source/pylock.rs` (`normalize_package_name`, used by `app_version`) is a
byte-identical **copy** of `ocx_python`'s private normalizer. `naming`'s function
is not part of the crate's public API; duplicating the ~12 lines is the deliberate
choice over widening the crate surface for one caller (documented in-line there).

### Convention 2 — Repack determinism

`repack::repack_wheel` writes one deterministic `tar.zst` layer per wheel, stamped
by `REPACK_VERSION = "repack-v1"`. Determinism knobs (`write_deterministic_tar_zst`):

- entries **sorted by path** (`tree.sort_by` before write);
- **epoch-0 mtimes** (`set_mtime(0)`), **uid/gid 0**;
- **normalized modes** — `MODE_FILE = 0o644`, `MODE_EXECUTABLE = 0o755` (the latter
  only for `.data/scripts` launchers);
- **pinned `ZSTD_LEVEL = 3`** (matches `ocx_lib::compression::CompressionLevel::Default`).

The layer digest is the sha256 of these bytes. The golden test
`golden_digest_is_stable_across_runs` pins the byte-identical output for the
`purelib_pkg` fixture to
`sha256:330a642c4e7fcc3a565889e85091f8397780a78ad360601c81fbe9e371cd8ebe` — any
drift is a determinism regression.

**Security guards** (both fail before an unbounded operation):

- **Zip-bomb** — `MAX_TOTAL_DECOMPRESSED_BYTES = 1 << 30` (1 GiB) across all
  entries, enforced by reading capped (`read_entry_capped` reads one byte over the
  remaining budget and aborts) → `RepackError::WheelTooLarge` (CWE-409). The
  budget is never near a real wheel; it exists only to abort a malicious/corrupt
  zip.
- **Zip-slip** — entry paths via `enclosed_name`, RECORD paths via
  `record_components`; an absolute path or `..` traversal is
  `RepackError::UnsafeEntryPath`.

`RepackError` maps to exit **1** (`ExecutionFailed`) — an I/O/zip fault, not
malformed data.

### Convention 3 — Layout (`.data` relocation → single content-root layer)

`repack::relocate` emits the **final relocated tree** so the layer needs no
placement metadata:

| Wheel-relative source | Relocated destination | Mode |
|---|---|---|
| `<dist>.data/scripts/*` | `bin/*` | `0755` (executable) |
| `<dist>.data/data/*` | content root (verbatim subpath; conventionally `share/…`) | `0644` |
| purelib / platlib / `<dist>.dist-info/*` | `lib/site-packages/*` | `0644` |
| `<dist>.data/{purelib,platlib,headers}/*` | `lib/site-packages/*` (fallback) | `0644` |

Because one wheel spans **three** destination prefixes (`lib/site-packages/`,
`bin/`, `share/…`) that a single layer prefix cannot express, each wheel becomes
**one content-addressed layer applied at the content root with an EMPTY
`LayerLayoutSpec`** (`compose::WheelLayer.layout` defaults empty; the field exists
only because `ocx_lib`'s layer-ref requires a spec, not to relocate wheels). The
tar already carries the final paths.

Env metadata baked by `compose` (`EnvBuilder`): `PYTHONPATH =
${installPath}/lib/site-packages` (**required**), `PATH = ${installPath}/bin`
(**optional** — a pure-python app whose only entrypoints are synthesized console
scripts ships no `bin/`), and `PYTHONDONTWRITEBYTECODE = 1` (keeps CPython from
writing `__pycache__/` into read-only package content).

### Convention 4 — Entrypoint synthesis + selection

**Which wheels' scripts are eligible (selection mode).** Which wheels'
`[console_scripts]` entries may synthesize is governed by `python.entrypoints:`
(mirror spec, `spec/python_config.rs`), resolved into
`ocx_python::EntrypointSelection` before `compose_env` runs — the crate itself
stays version-agnostic; the mirror resolves any version-windowed config against
the concrete app version first (`PythonConfig::resolve_entrypoint_selection`).

| Mode | Spec value | `EntrypointSelection` | Admits |
|---|---|---|---|
| `auto` (**default**, `plan_python_mirror_v2`) | `entrypoints: auto` or omitted | `RootOnly { root_package }` | Only the root package's own console scripts, matched by PEP-503-normalized dist name against the wheel's parsed filename |
| `all` | `entrypoints: all` | `All` | Every wheel's console scripts — the pre-`plan_python_mirror_v2` behavior |
| explicit list | `entrypoints: [{name, min_version?, max_version?}, …]` | `Explicit(names)` | Only the listed names; each entry optionally windowed to an app-version range (`min_version` inclusive / `max_version` exclusive — same convention as `versions:` and per-platform `min_version`/`max_version`) |

`auto` is the new default: previously every wheel's scripts synthesized
unconditionally (now the `all` mode). Rationale: a transitive dependency
shipping its own CLI (e.g. a linter dependency's `foo-lint` script) is rarely
something the *published app* wants surfaced as one of its own entrypoints.

**Fail-closed collision + miss (replaces silent last-write-wins).** Synthesized
entrypoints previously landed in a plain `BTreeMap`, so two wheels providing the
same script name silently resolved to whichever inserted last. This is now
fail-closed:

- **`ComposeError::EntrypointCollision { name, first_wheel, second_wheel }`** —
  a genuine cross-wheel name clash (two *admitted* wheels providing the same
  console-script name) aborts composition, naming both wheels, instead of
  silently keeping one.
- **`ComposeError::MissingEntrypoint { name }`** — under `Explicit`, a listed
  name that no admitted wheel actually provides aborts composition, instead of
  silently composing an env missing a requested launcher.

**V3 finding — spawn-parity and the `RootOnly` caveat.** `plan_python_mirror_v2`
W0.1 (finding V3, `.claude/state/plans/notes_v1v2v3_python_mirror_v2.md`)
verified that a synthesized entrypoint is **not** metadata-only: OCX
materializes a real, executable POSIX-shell launcher shim per entrypoint under
the composed package's `entrypoints/` directory, and that directory sits on
`PATH` in every composed env (`ocx run`, `ocx package exec`, `ocx package
test`) — identical to `content/bin/*`. Concretely: `subprocess.run(["pycowsay",
"moo"])` from inside a composed env's `python3` succeeds exactly as it would
against a normally pip-installed console script (verified end-to-end via `ocx
package test`: a Starlark script asserting the spawn *fails* had its own
assertion fail, because the spawn succeeded).

Consequence for `RootOnly` (the new default): an application that itself spawns
a **dependency's** console script by name (e.g. a wrapper CLI shelling out to a
bundled linter) will no longer find it under `auto`, because `RootOnly` only
ever admits the root package's own scripts. Such an app needs `all` or an
explicit entry naming the dependency's script — `auto`'s root-only default is a
behavior change for this (narrow) class of app, not just a metadata-visibility
change.

**The previously mooted `bin_shims` knob is REJECTED.** A separate
configuration knob to keep launcher shims for non-admitted dependency scripts
on `PATH` (without registering them as full OCX entrypoints) was floated during
planning as a middle ground for the `RootOnly` default. V3 refutes its premise:
a *registered* entrypoint already gets a real, spawnable `PATH` shim — there is
no metadata-only tier to add a knob for. The actual gap `RootOnly` creates is
entrypoint **admission** (does compose synthesize a launcher at all), not
launcher **mechanism** (how the launcher dispatches once synthesized) — and
`all`/`Explicit` already close the admission gap. `bin_shims` would have added
a second, redundant way to admit a script's launcher without registering it as
an entrypoint, solving a problem V3 showed does not exist.

**Shim synthesis mechanics.** `compose_env` synthesizes one entrypoint per
**gated and admitted** `[console_scripts]` entry, each
`{ command: "python3", args: ["-c", <shim>] }` (argv array, no shell). The
shim (`synthesize_shim`) is:

```python
import importlib, sys
sys.argv[0] = "<name>"
_obj = importlib.import_module("<module>")
_obj = getattr(_obj, "<attr>")   # repeated per attr in the dotted chain
sys.exit(_obj())
```

- Resolution is `importlib.import_module` + a `getattr`-walk over the attr chain —
  **never** a `from … import …` template, which cannot express a dotted attribute
  reference (`pkg.mod:Class.method`).
- **`sys.argv[0] = "<name>"`** so click/argparse report the real program name.
  Regression fixed: without it a process saw `argv[0] == "-c"` and printed
  `-c, …` in `--version`/`--help`. The name is validated as an `EntrypointName`
  (`^[a-z0-9][a-z0-9_-]*$`) before it is embedded, so no escaping is needed.
- A malformed reference (empty module/attr, more than one `:`) is
  `ComposeError::InvalidEntryPoint`.

**A bare `python3` entrypoint (`{ command: "python3", args: [] }`) is ALWAYS
synthesized** (`entries.entry(python3).or_insert_with(…)`). A LIBRARY env with no
console script (e.g. `google-cloud-aiplatform`) is otherwise unrunnable; a plain
`ocx run <env> -- python3 …` override does **not** get the package's private env
(PYTHONPATH), so imports fail. Dispatching `python3` as an entrypoint runs the
composed interpreter **with** the env applied — the only way to `import` the
library. Insertion is skipped if a wheel already shipped a `python3` console
script (never observed; fail-safe).

**Extras gating.** A script is synthesized only when **every** extra it is gated on
is in `EnvSpec.requested_extras`
(`script.extras.iter().all(|e| requested_extras.contains(e))`); the empty gate is
always synthesized. So `blackd = blackd:main [d]` is **not** synthesized for plain
`black` (extra `d` unrequested). Requested extras are validated against the lock's
declared `extras` (`EnvSpec.declared_extras`); an unknown extra is
`ComposeError::UnknownExtra` — a typo fails closed rather than registering an
unresolvable launcher. Gating is never inferred from dependency presence.

### Convention 5 — Platform / axis encoding

> **Amended 2026-07-09 — L2 superseded by `os.features` platform keys.** The L2
> facts→variant-prefix encoding below (`encode_l2` / `encode_platform_key` /
> `encode_variant_prefix`, grammar `L2_GRAMMAR_VERSION = 1`) is **deleted**. The
> published platform is now the maintainer's declared `wheels:` key — the spec
> surface, verbatim, including an optional `+libc.glibc`/`+libc.musl`
> `os.features` suffix (ocx ≥ 0.4.2 `can_run` subset matching). Env packages
> publish bare tags with one image index whose entries differ by `os.features`;
> the variant tag prefix no longer exists for env sources. `wheel_priority` on
> `VariantConstraints` is now an admissibility filter + ranking (non-empty list
> excludes non-matching wheels), derived per key from the `wheels:` filter. L1
> (`parse_platform_tag`) is unchanged and remains the frozen fact table.

A Python target is 5-axis `(os, arch, libc{family,floor}, python, abi)`; an OCX
`Platform` carries os/arch only. `platform.rs` layers the mapping:

- **L1** (`parse_platform_tag`) — a PEP 425/600/656 wheel tag → `PlatformFacts`;
  frozen fact table (e.g. `manylinux_2_28_x86_64` → `{Linux, Amd64, gnu≥2.28}`).
- **L2** (`encode_l2`, grammar `L2_GRAMMAR_VERSION = 1`) — os/arch →
  `ocx_lib::oci::Platform` (`encode_platform_key`); libc/ABI → a mirror **variant
  tag prefix** (`encode_variant_prefix`): default (glibc + primary ABI) → `None`
  (unadorned), `musl` libc → `"musl"`, ABI override → e.g. `"cp313t"`, both →
  `"musl-cp313t"`. v1→v2 (the planned `+libc` platform grammar) is a republish; L1
  facts are stable across both.

The **variant axis** (`VariantConstraints`) is bounded to L1 fact fields — libc
family, `min_manylinux` / `min_musllinux` floors, and an ABI override — never a
free-form tag regex. `encode_variant_prefix` rejects internally inconsistent
variants (`musl` libc with a `manylinux` floor, or a `musllinux` floor without a
`musl` libc) as `PlatformError::InvalidVariant`.

**ABI consistency at compose (fail closed).** `compose::check_abi` requires every
wheel's ABI to be universal (`none`, `abi3`) or exactly the target's
**effective ABI** — `PythonTarget::effective_abi()` = the variant override else the
interpreter pin. A concrete `cpXY(t)` that differs (e.g. a `cp313` wheel against a
free-threaded `cp313t` interpreter), or an unparseable wheel filename, is
`ComposeError::AbiMismatch` (exit 65). `select` applies the same invariant earlier
via `validate_abi_consistency` (`SelectError::AbiMismatch`, comparing the
`gil_disabled` flag), so drift is caught at both selection and composition.

### Convention 6 — Marker-env / wheel selection

`select::select_wheels(lock, target)` picks exactly one wheel per applicable
package for one `(variant, platform key)` target:

1. **Marker environment** — `platform::marker_environment(facts, interpreter)`
   builds the versioned `MarkerEnvironment`; `select` converts it into a
   `uv_pep508::MarkerEnvironment` (`build_marker_environment`).
2. **Package filter** — evaluate each package's PEP 508 marker
   (`package_applies`); non-applicable packages (OS forks, implementation forks)
   are **dropped**, not failed. A malformed marker is `SelectError::MarkerSyntax`.
3. **Target tag set** — `Tags::from_env` from `uv-platform-tags` (python/abi +
   libc-floored platform), so `abi3` spanning minors, `py2.py3-none-any`, and
   `any` fall out of **tag-compatibility semantics, never string equality**.
4. **Rank** — `pick_wheel` keeps `TagCompatibility::Compatible(priority)`
   candidates, best by (tag priority, PEP 427 build tag, filename) — the last two
   deterministic tiebreaks.
5. Zero compatible wheels for an applicable package →
   `SelectError::NoCompatibleWheel` naming package, target, variant, and the tags
   that **were** available (distinguishing psycopg2-style "no wheel for this
   triple" from uwsgi-style "no wheel anywhere") → **exit 65**. A selected wheel
   with no URL is `SelectError::MissingUrl` (not mirrorable).

**Marker-env table** — the values `marker_environment` derives for the reference
target **CPython 3.14.6 / cp314 on linux-amd64**:

| Marker variable | Value | Source in `platform.rs` |
|---|---|---|
| `python_version` | `3.14` | `interpreter.python_version` |
| `python_full_version` | `3.14.6` | `interpreter.python_full_version` |
| `sys_platform` | `linux` | OS-axis map (Linux) |
| `platform_system` | `Linux` | OS-axis map (Linux) |
| `os_name` | `posix` | OS-axis map (Unix-like) |
| `platform_machine` | `x86_64` | `platform_machine(Linux, Amd64)` |
| `implementation_name` | `cpython` | `Implementation::CPython` |
| `platform_python_implementation` | `CPython` | `Implementation::CPython` |

(`platform_machine` is OS-dependent: Linux/macOS report `x86_64`/`aarch64`/`arm64`,
Windows reports `AMD64`/`ARM64`. `select` additionally passes
`implementation_version = python_full_version` and leaves `platform_release` /
`platform_version` empty.)

### Convention 7 — Collision / overlap-free union

OCX's prefix-layer union is **overlap-free by design**, so a valid resolved lock
composes a correct `site-packages` by construction. `collide::check_collisions` is
the pre-publish guard proving the invariant for a concrete wheel set: two repacked
wheels claiming the same installed (post-relocation) path is
`CollisionError::OverlappingPaths` naming the path and both wheels → **exit 65**,
failing before push rather than corrupting the registry.

**PEP 420 namespace dirs are shared safely.** A wheel's `RECORD` lists only files,
never bare directories, so two dists contributing distinct leaves under the same
namespace directory (`google/cloud/foo/__init__.py` vs
`google/cloud/bar/__init__.py`) never produce equal path strings and never collide.

**Coexisting-hostile-distribution caveat.** Some `[extras]` closures pull
mutually-exclusive dists that ship a **byte-identical same-path** file — observed:
`mlflow` / `mlflow-skinny` / `mlflow-tracing` all ship an identical
`mlflow/__init__.py`. OCX's overlap-free model rejects this rather than tolerating
overlap (even byte-identical overlap). The resolution is **lock curation** — exclude
the redundant subset with `uv … --no-emit-package` and keep the superset — i.e.
respecting OCX's model, not fighting it with a merge/dedupe special case.

### Convention 8 — libc-variant interpreter provisioning & container validation

> **Amended 2026-07-09 — variant axis superseded.** Env packages no longer have
> variants: libc is declared on `wheels:` platform keys and published as OCI
> `os.features`. The per-variant `interpreter_package` override is **deleted** —
> a single `python.interpreter_package` serves every key (a dual-libc app needs
> an interpreter whose own index resolves per-libc, or a musl/static build that
> runs on both). Container validation below still stands, now realized as
> libc-compatible JUnit *gating*: a `+libc.musl` entry is gated by musl (alpine)
> legs only, `+libc.glibc` by gnu legs, and a featureless entry by ALL legs of
> its base platform.

Convention 5 encoded libc as a **variant** (a tag prefix), never an `os/arch`
platform key. Its mirror-side realization pinned two one-way-door registry
conventions:

**Interpreter provisioning.** An env's private interpreter dependency is resolved
**per variant**. The default (glibc) variant uses `python.interpreter_package`
(the stock `ocx.sh/cpython:<ver>` — a glibc/manylinux CPython). A `libc: musl`
variant overrides it with a per-variant `interpreter_package` pointing at a
**musl-libc CPython published to a *separate repository*** —
`dev.ocx.sh/ocx/cpython-musl:<ver>` — not a musl candidate inside the glibc
`cpython` index. This is forced by OCX's platform model: index candidates are
keyed by `os/arch` only, so a musl and a glibc `linux/amd64` cannot coexist under
one tag; libc is the variant axis, so the two libc builds are two repos. The musl
interpreter is a plain single-layer archive package (`install_only` tarball,
`strip=1`, `PATH=${installPath}/bin`) — the same shape as the glibc one, no
entrypoints. `VariantSpec.interpreter_package` (mirror spec) is the seam;
`Source::Pylock.package` lets a `pycowsay-musl` mirror resolve the `pycowsay`
package from a shared lock without a name collision.

**Container validation.** A libc-variant env is validated in a libc-matched base
image: the `alpine` leg exercises the `musl` variant end-to-end, the `debian` leg
sanity-checks the glibc floor (an older glibc than the CI runner). The generated
workflow keeps the job on the host runner (JS actions need the glibc node GitHub
mounts, which Alpine's musl userland cannot run) and wraps only `ocx package test`
in `docker run <image>` with a statically-linked, **libc-matched ocx release
binary** mounted in (musl for `alpine*`, gnu otherwise). The runner CA bundle is
mounted at `/etc/ssl/certs/ca-certificates.crt` because the gnu ocx verifies TLS
against the system store, which a minimal image omits (the musl ocx bundles webpki
roots). The env under test is self-contained (local wheel layers); only its
private interpreter is pulled — anonymously — from the registry.

## Consequences

**Positive:**

- **Deterministic** — pinned repack knobs + the golden digest test make the same
  wheel produce the same layer bytes on every run and every writer.
- **Content-addressable** — wheels are named by `sha256`; identical blobs dedup,
  re-runs are no-ops.
- **Target-agnostic** — the crate emits `Metadata` + `Platform` (the two
  registry-independent thirds of an `Info`) and repo-relative identifiers; it never
  constructs a registry-bearing `Identifier`. `EnvComposition::into_info(identifier)`
  is the **sole seam** where the consumer injects the registry host, so the same
  translation logic serves `ocx-mirror` and `ocx-dist` unchanged.
- **Fail-closed** — malformed locks, ABI drift, path overlap, and zip-bombs abort
  before push (exit 65 for data faults, 1 for I/O), never at the user's runtime.

**Negative:**

- Any convention change (repack layout, L2 grammar) is a **republish** event, not
  an in-place edit — hence the `REPACK_VERSION` / `L2_GRAMMAR_VERSION` stamps.
- The mirror-side normalizer is duplicated (Convention 1); a PEP 503 change must be
  applied in two places until the normalizer is promoted to the crate's public API.

**Risks:**

- Convention drift between writers corrupts the shared namespace irreversibly.
  Mitigation: this ADR + the golden/marker-env/encode tests are the guard; the L1
  fact table and L2 encoding are frozen and versioned in code.

## Validation

- [x] Unit tests cover each convention: `golden_digest_is_stable_across_runs`
  (repack), `parse_*` / `encode_l2_*` / `marker_env_*` (platform), `select_*`
  (select), `*_collision*` (collide), shim/extras/ABI/selection tests incl.
  `EntrypointCollision`/`MissingEntrypoint` (compose),
  `normalizes_package_names_per_pep_503` (naming).
- [x] Security guards tested: `zip_slip_entry_is_rejected`,
  `zip_bomb_decompressed_size_is_capped`.
- [ ] Acceptance corpus (design spec "Testing strategy") exercises the conventions
  end-to-end per platform leg.

## Links

- [design_spec_ocx_python.md](./design_spec_ocx_python.md) — component design spec (primary source)
- [adr_ocx_python_crate.md](./adr_ocx_python_crate.md) — the workspace/boundary decision
- [adr_pypi_lock_derivation.md](./adr_pypi_lock_derivation.md) — the corpus interpreter model + V1c/V3 evidence Convention 4's revision cites
- [research_python_wheel_oci.md](./research_python_wheel_oci.md) — background research
- [subsystem-mirror.md](../rules/subsystem-mirror.md) — module map + error model
- `.claude/state/plans/notes_v1v2v3_python_mirror_v2.md` — V3 spawn-parity evidence record; gitignored, cited by path

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-07-04 | pylock-mirror swarm | Initial draft — seven implemented conventions |
| 2026-07-04 | pylock-mirror swarm | Convention 8 — libc-variant interpreter provisioning (separate `cpython-musl` repo, per-variant `interpreter_package`) + container test-leg validation (alpine/musl, debian/glibc floor) |
| 2026-07-05 | pylock-mirror swarm (w3-adr) | Convention 4 revision — entrypoint selection modes (`auto`/`all`/explicit windowed list), fail-closed `EntrypointCollision`/`MissingEntrypoint` replacing silent last-write-wins, V3 spawn-parity finding + `RootOnly` caveat, `bin_shims` knob rejected |
| 2026-07-09 | python-mirror-v2 | Conventions 5/8 amended — variant axis superseded by `wheels:` platform keys with `os.features` libc (v0.4.2 `can_run`); L2 encode surface deleted; `wheel_priority` promoted to admissibility filter + ranking; single `python.interpreter_package` (per-variant override deleted); libc-compatible JUnit gating at push |
