# Research: Python wheels → OCX packages (pylock-driven)

<!--
Condensed from session "pip-mirror" (2026-07-03/04): three research fan-outs
(OCX composition internals, Python packaging standards, Rust libs/prior art)
plus two empirical investigations (uv install model, multi-root import model).
Empirical claims were verified by running uv 0.11.19 / Python 3.14.5 locally.
-->

## Question

Can a uv.lock/pylock.toml-locked Python application be mirrored into an OCI
registry as composable OCX packages — wheel-granular, content-addressed,
reconstructible into a runnable environment without pip/uv at runtime?

## Key findings

### F1 — A valid lock is collision-free by construction (empirical)

Installed a real 20-package lock (google-api-core, google-auth,
google-cloud-core, markupsafe, certifi, tabulate + transitive) into one venv:
840 distinct install paths, 5 dists writing under `google/`, **zero paths
owned by two dists**. pip/uv reject two packages owning the same path at
resolve time. Consequence: OCX's overlap-free layer union (dirs merge,
same-path file = `LayerOverlap` error) is *exactly correct* for a locked
wheel set — the fail-closed error becomes a free integrity check.

### F2 — OCX ≥ v0.4.x provides the two required primitives

- **Per-layer placement** (`c26f362`, upstream): `sh.ocx.layer.strip-components`
  + `sh.ocx.layer.prefix` layer-descriptor annotations; publish grammar
  `ocx package push <ref>:strip=N,prefix=P`; resolved at pull, applied before
  the overlap merge; path-safety validated at publish and read boundaries.
- **Entrypoint args** (`048dcea`, upstream): `Entrypoint { command, args }`;
  dispatch command resolved **against the composed PATH from the package's
  env block**; args support `${installPath}` (`${deps.*}` rejected in args,
  allowed in env values).

Together: N wheel layers with `prefix=lib/site-packages` union into one
site-packages; `command: python3` resolves to a private interpreter
dependency on PATH. **Zero OCX core changes needed.**

Constraint: pull path accepts tar layers only (`compression.rs::from_media_type`
= tar+{gz,xz,zstd}); wheels are zip → mirror must repack wheel→tar.zst.
Repack must be deterministic (sorted entries, epoch mtimes) so identical
wheels yield identical layer digests.

### F3 — What an installer does beyond unzip (empirical, uv 0.11.19)

`uv pip install --target DIR` = flat PYTHONPATH-able root. Beyond unzip:
RECORD rewrite (runtime-irrelevant), console-script synthesis from
`entry_points.txt` (wrappers do NOT exist in the wheel; shebang points at the
installing interpreter), `.data/` scheme relocation (scripts→`bin/`,
data→root), optional byte-compile. uv materializes by **hardlinking from its
content-addressed cache** (verified same inode; Linux default) — uv's model
converges with OCX's hardlink CAS.

Runtime import needs none of the installer steps: multi-root and merged-root
both import cleanly (pure-python, C-extension `.so`, PEP 420 namespaces
merging across roots). `.pth` side effects don't fire on raw PYTHONPATH but
modern locks don't need them (legacy `pkg_resources` nspkg is dead weight on
Python ≥3.3; setuptools not even importable in base 3.14).

### F4 — Platform matrix is irreducible

One composed env = one `(python-tag, abi-tag, platform-tag)` triple.
manylinux (PEP 600, glibc floor) vs musllinux (PEP 656) cannot coexist.
Free-threaded `cp313t` ABI must match the interpreter build. Draft PEP
817/825 ("Wheel Variants": CPU microarch/CUDA axes) will widen the matrix.
Model like OCI multi-arch: N parallel compositions, platform-slugged tags.

Interpreter ships separately: python-build-standalone (Astral-stewarded),
keyed by version+platform+variant — a second pinned hash that must agree on
ABI with the wheel set.

### F5 — Lock format: PEP 751 is the stable target

PEP 751 `pylock.toml` = Final; hashes per artifact under
`packages.*.wheels[].hashes`; `environments` + per-package `marker` encode
platform forks. Exporters: uv (`uv export --format pylock.toml`), PDM, pip
(experimental). Poetry: none as of 04/2026. uv.lock itself = proprietary,
versioned, breaking-change cadence — do not parse it; require pylock.

### F6 — Rust reuse: uv parser crates, git-pinned only

`uv-distribution-filename` (PEP 427/425 filename→tags) and
`uv-platform-tags` (tag compatibility) are the only correctness-critical
parsers worth importing. crates.io releases are explicitly "internal
component crate of uv", 0.0.x every few days, zero external consumers →
**git-pin to a rev** (same discipline as the ocx submodule), never a version
range. Standalone predecessors (pep440_rs, pep508_rs, install-wheel-rs) are
stale/dead. Market signal: pixi killed its own resolver ("rip") and embeds
uv's crates.

### F7 — Prior art

- **PyOCI** (Rust, MIT): wheels/sdists as OCI artifacts, hash dedup,
  registry-as-index. Closest prior art for the mirror half; no lock/env
  concept. Read its annotation schema before finalizing ours.
- **rattler CAS proposal** (conda/rattler#1383): hash-sharded hardlink store
  blueprint.
- **monotrail**: proved per-wheel-dir sys.path model at scale; author then
  built uv with merged site-packages — merged root is the production shape.
- **uv2nix**: validates lock→content-addressed-env, but rides the Nix store.

### F8 — Test corpus (verified against live PyPI, 2026-07)

19 edge-case properties catalogued (full table in the corpus research
session). Canonical 5-app corpus + 2 negative fixtures:

| Tier | App | Uniquely covers |
|---|---|---|
| easy | `pycowsay 0.0.0.2` | zero-dep baseline, 1 console_script, 1 layer |
| medium | `yt-dlp 2026.6.9` | implementation-marker fork (`brotli` vs `brotlicffi`), zero mandatory deps + optional C-ext set, fast release cadence |
| medium | `black 26.5.1` | per-interpreter mypyc wheels (cpXY) with `py3-none-any` fallback, 2 console_scripts, one extras-gated (`blackd [d]`) |
| hard | `streamlit 1.58.0` | numpy+pandas+pyarrow+pillow unconditional, inverted marker (`watchdog; platform_system != "Darwin"`), real CLI |
| hard | `google-cloud-aiplatform[full] 1.159.0` | PEP 420 namespace union at scale, legacy `py2.py3-none-any` tag union, 150–200+ wheels |
| negative | `uwsgi` | never ships wheels → "no wheel exists anywhere" failure path |
| negative | `psycopg2` (non-binary) | zero Linux wheels → per-triple selection-failure path |

Targeted unit fixtures (one property each): `cryptography`/`bcrypt` (abi3
spans python minors), `ruff` (maturin `bindings=bin`, binary in
`.data/scripts`, widest wheel matrix: 14 platforms), `jupyterlab`
(`.data/data` → `share/`, installs OUTSIDE site-packages), `mkdocs-material`
(`mkdocs.plugins` entry-point group + `cairosvg` unvendored system-lib
hazard), legacy `*-nspkg.pth` pins, `httpie`/`pipx` (win32 markers).

**Design-relevant hazards found:**
- **Runtime writes into site-packages**: CPython writes `__pycache__/*.pyc`
  on first import (universal); `numba` JIT-caches next to source in
  site-packages. Read-only hardlink-CAS content breaks silently →
  mitigation required (`PYTHONDONTWRITEBYTECODE=1` in env metadata, or
  pre-baked hash-based pyc per PEP 552 later).
- **`.data/data` lands outside site-packages** (`sys.prefix/share/...`) —
  "union into one site-packages" is not the whole story; package content
  root needs `share/` etc. as first-class merge targets.
- **Tag unions** (`py2.py3-none-any`, `abi3`): selection must use PEP 425
  compatibility semantics, never string equality.
- **Extras-gated console_scripts** (`blackd = ... [d]`): synthesize
  entrypoints only for scripts whose extra is actually locked.

### F9 — Lock provenance for already-published wheels

Published wheels/sdists ship **no lock** — `Requires-Dist` = version ranges;
PEP 751 is a consumer-side artifact. A July 2025 pre-PEP ("pylock inside
wheels") got a strong -1 and was superseded by another still-unaccepted
draft — do not design against locks-in-distributions landing.

Deriving a lock for a published app at a pinned version is a verified
one-shot:

```sh
echo "black==26.5.1" | uv pip compile - --format pylock.toml \
  --python-platform x86_64-unknown-linux-gnu --python-version 3.12 \
  --exclude-newer 2026-07-01T00:00:00Z -o pylock.toml
```

`--exclude-newer` (PEP 700 upload-time cutoff) makes re-resolution
reproducible over time. Caveats: one lock per triple (or `--universal` +
marker filtering); yanked-file risk not covered by the cutoff.

## Implications for design

1. Wheel-granular mirroring + prefix-layer composition is feasible on
   upstream ocx ≥ v0.4.1 with zero core changes; submodule bump is a prereq.
2. The translation layer (lock→selection→repack→composition) is pure and
   consumer-independent → extract as `ocx_python` library crate; registry
   I/O stays in consumers (ocx-mirror, future ocx-dist).
3. Cross-repo conventions (naming, repack determinism, prefix layout,
   entrypoint synthesis) are one-way-doors → dedicated ADR, single code
   implementation in `ocx_python`.
