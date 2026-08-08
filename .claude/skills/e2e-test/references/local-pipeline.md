# Tier 1 — Local integration against the contrib fleet

You loaded this file to run the current `main` binary against real
`~/dev/ocx-contrib` specs with all registry writes confined to
`localhost:5001`. Network reads (GitHub API, PyPI, ocx.sh anonymous pulls)
are allowed; nothing is published to a real registry.

Every command below was executed and verified on 2026-08-08.

Contents: [Setup](#setup) · [Fleet sweep](#fleet-sweep-offline) ·
[Archive/binary pipeline](#archivebinary-pipeline) ·
[Python env pipeline](#python-env-pipeline-pipx) · [Gotchas](#gotchas) ·
[What this tier proves](#what-this-tier-proves--doesnt)

## Setup

```sh
cargo build --release                                # binary under test
docker compose -f test/docker-compose.yml up -d      # registry:2 on :5001
export OCX_INSECURE_REGISTRIES=localhost:5001
export GITHUB_TOKEN=$(gh auth token)                 # github_release plan
BIN=$PWD/target/release/ocx-mirror
```

`ocx` ≥ 0.5.5 must be on PATH (`ocx version`); `uv` needed for pypi specs.
Work in a scratch dir — never modify ocx-contrib, never commit there.

## Fleet sweep (offline)

~115 specs / ~94 repos. Both commands are read-only (`--check` proven
non-mutating).

```sh
# validate every spec — spec path is POSITIONAL, --spec is rejected here
find ~/dev/ocx-contrib -maxdepth 3 -name mirror.yml | while read -r s; do
  (cd "$(dirname "$s")/.." && $BIN package validate "${s#*/mirror-*/}") \
    || echo "FAIL $s"
done

# drift per repo — run from repo root, one --spec per package (copy the
# exact command from the repo's committed verify-generated.yml)
(cd ~/dev/ocx-contrib/mirror-<x> && $BIN package pipeline generate ci --check \
  --spec <pkg>/mirror.yml)   # exit 0 clean, 65 drift, other = renderer error
```

Universal drift after template changes is expected and informational; a
non-0/65 exit or a validate failure is the real signal.

## Archive/binary pipeline

Small proven specs: `mirror-sharkdp/hexyl` (~550 KB archives),
`mirror-jqlang/jq` (`asset_type: binary` — the no-extraction path).

1. Copy `mirror-base.yml` + `<pkg>/mirror.yml` into scratch preserving the
   `extends: ../mirror-base.yml` layout (plus `metadata.json`, `logo.svg`,
   `tests/` so relative refs resolve).
2. Patch the copy: `target:` → `localhost:5001` / `it/<name>`; trim
   `platforms:`/`assets:` to `linux/amd64` (drop per-platform
   `min_version` blocks referencing dropped keys); raise `versions.min`
   to admit 1–2 versions; delete `announce:` (push degrades gracefully
   without notify/announce credentials).
3. Run from one fixed dir:

```sh
$BIN package pipeline plan --spec spec/<pkg>/mirror.yml --format json > plan.json
V=$(jq -r '.versions[0].version' plan.json)          # e.g. 0.17.0_20260808
$BIN package pipeline prepare --spec spec/<pkg>/mirror.yml \
    --version "$V" --plan plan.json --work-dir ./work
# flatten the way generated CI does (prepare_flatten_script in
# generate/ci/matrix.rs): slug = version with + -> _
mkdir -p bundles junit
cp work/${V/+/_}/linux_amd64/bundle.tar.xz  bundles/bundle-$V-linux_amd64.tar.xz
cp work/${V/+/_}/linux_amd64/metadata.json  bundles/bundle-$V-linux_amd64-metadata.json
# one green JUnit per container leg: junit-<V>-<platform_slug>-<container_id>.xml
# container_id: ubuntu:24.04 -> ubuntu_24_04; testcase name MUST equal tests[].name
$BIN package pipeline push --spec spec/<pkg>/mirror.yml \
    --junit-dir ./junit --bundles-dir ./bundles --write-summary ./run-summary.json
```

Assert: `run-summary.json` `status: published` + `cascade_tags_written`;
`curl -s localhost:5001/v2/it/<name>/tags/list` shows version + cascade;
every cascade alias resolves to the same index digest. Runtime proof:
`ocx package pull localhost:5001/it/<name>:latest` into a throwaway
`OCX_HOME`, run the binary.

## Python env pipeline (pipx)

Recreates the `mirror-pypa/pipx` spec (`source.type: pypi`) — the only
acceptance-level test of env-package `push` that exists (the in-repo pytest
suite stops at `prepare`).

Patch as above, plus: `python.interpreter_package:` →
`localhost:5001/astral-sh/python-build-standalone:3.13.14`; `wheels:` keep
only `"linux/amd64+libc.glibc": ~`; one `ubuntu:24.04` container leg.

**Interpreter first** — real copy beats the stub (enables runtime proof):

```sh
OCX_HOME=$(mktemp -d) ocx package pull \
    ocx.sh/astral-sh/python-build-standalone:3.13.14 -p linux/amd64+libc.glibc
# pull keeps the EXTRACTED tree only — re-tar it. List top-level entries
# explicitly: `tar -C content -cf - .` emits a ./ root member that later
# breaks `ocx package pull` of the composed package ("tar error: No such
# file or directory ... creating dir .../content/.").
tar -C <pulled>/content --numeric-owner -cf - bin include lib share \
  | zstd -3 -T0 -o interp.tar.zst
ocx --format json package push -p linux/amd64+libc.glibc \
    -i localhost:5001/astral-sh/python-build-standalone:3.13.14 \
    -m <pulled>/metadata.json interp.tar.zst
```

(Fallback: 1-layer stub as in `test/src/helpers.py::push_stub_ocx_package` —
prepare only resolves the manifest digest for universal locks — but then
skip the runtime proof.)

```sh
$BIN package pipeline plan --spec spec/pipx/mirror.yml \
    --locks-dir ./locks --format json > plan.json    # real uv lock per candidate
# plan.json and locks/ MUST stay siblings — pylock paths resolve against
# plan.json's directory
$BIN package pipeline prepare --spec spec/pipx/mirror.yml \
    --version <V> --plan plan.json --work-dir ./work
mkdir -p bundles && cp -R work/<V> bundles/<V>       # env subtree travels whole
# junit-<V>-linux_amd64-ubuntu_24_04.xml — BASE slug, no libc suffix
$BIN package pipeline push --spec spec/pipx/mirror.yml \
    --junit-dir ./junit --bundles-dir ./bundles --write-summary ./run-summary.json
```

Assert: `run-summary.json` `layer_reuse.mounted == <wheel count>` (wheel
layers cross-repo-mounted, not re-uploaded); tags on `it/pipx`; one
`pip-packages/files.pythonhosted.org/<name>` repo per wheel, tagged by the
wheel's sha256. Runtime proof: pull the package, `ocx run -- pipx
--version`, and `pipx environment --value PIPX_DEFAULT_PYTHON` must point
inside the private interpreter package, not the host.

## Gotchas

- `package validate` takes the spec **positionally**; `--spec` is a flag
  only on the `pipeline` subcommands.
- `ocx-mirror` has no `--version` flag.
- rtk-filtered `git status`/diff output is unreliable for load-bearing
  checks — use `rtk proxy git …`.
- Registry state persists across runs — re-pushing a version re-points
  bare tags (see F2 in the 2026-08-08 findings). Use fresh `it/*` repo
  names per run, or wipe the registry volume.

## What this tier proves / doesn't

Proves: spec parsing of the real fleet, plan against live sources, real
downloads, bundle/compose, push + cascade math + wheel-layer registration,
JUnit verdict wiring, runtime viability of published packages.
Does NOT prove: real `ocx package test` container legs (JUnit is
fabricated green — a red container matrix like pipx's F1 only shows up in
Tier 2 or a manual `ocx package test`), multi-platform index merge,
`+libc.*` gating across legs, announce/index PRs, GHA workflow semantics.
