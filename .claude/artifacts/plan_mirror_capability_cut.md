# Register: mirror capability cut

Durable state for the current initiative. Lives in `.claude/artifacts/` (committed) and
**not** `.claude/state/` (gitignored) precisely so it survives a lost session. Update the
Status block and the capability table in the same commit as any work below.

## Status

- **Initiative:** mirror-capability-cut
- **Active step:** 1 — metadata-only re-publish (`ocx-mirror#9`)
- **Step state:** decisions settled, ready to plan
- **Last update:** 2026-07-28 (after 2736cd1: chore(claude): point the register at the branch docs and the serve tasks)

## Goal

Get `ocx-mirror`'s feature set to a cut where a maintainer can say "this is the mirror
functionality, and it fits". Proven on two pilots, in order:

1. **`bazelbuild/buildtools`** — multi-binary release, and the concrete metadata-backfill case.
2. **`astral-sh/python-build-standalone`** — large platform matrix.

## Explicitly NOT in scope

- Adopting the existing mirrors. The owner is teaching a separate AI skill for that; this
  initiative does not touch repos beyond the two pilots.
- `ocx-mirror#18` (republish to `ghcr.io/ocx-contrib/*` + two-segment names).
- `ocx-sh/ocx#251` (delete the index flat-name fallthrough). Downstream of #18; see
  `~/.claude/plans/atomic-plotting-deer.md` for the full trace of why.

Anything phrased as "the fleet" is downstream of this cut, not part of it.

## Capability status (audited 2026-07-28)

| Capability | ocx core | ocx-mirror | Tracking |
|---|---|---|---|
| Many binaries in one package | **Ships.** `entrypoints` is a `BTreeMap`, `binaries` a `BTreeSet`, no arity cap (`package/metadata/bundle.rs:42-79`); one launcher per entry (`launcher/generate.rs:64`); `push` takes N layers with per-layer `strip=`/`prefix=` (`publisher/layer_ref.rs:134-152`); `LayerRef::Digest` reuses a published layer via `head_blob`, no upload (`oci/client.rs:1020-1049`) | **Missing.** `mirror.yml`'s resolver errors `Ambiguous` on 2+ distinct assets per platform (`src/resolver.rs:14-19`); `asset_type.binary.name` is a single `String`. One spec = one binary family | #16 |
| Metadata-only re-publish over a version range | **Ships.** Metadata lives in the config blob, not a layer — `ocx package push -i <tag> sha256:<hex>.tar.xz --metadata new.json` re-uploads only the config blob + manifest. `ocx#164` closed COMPLETED on exactly this | **Missing.** No `patch.rs` in `src/command/package/pipeline/`; every sync is a full download → bundle → push. No range grammar anywhere in either repo (no `semver` crate, zero `VersionReq` hits); `--version` is an exact-string `Vec<String>` | #9 |
| Announce from registry | **Ships** — `1deddbea`, flag `--tags-from-registry` (`--tags-file` renamed `--tags-from-file`) | **Ships on main** — `0df96e4`, `e34b410`, `339b9b2`: `pipeline announce` + the `workflow_dispatch`-only `announce-from-registry.yml` | — |

Constraint worth remembering: a package's N layers must not overlap on assembled paths —
overlap is rejected at install time.

## Steps

1. **`ocx-mirror#9` — `pipeline patch --metadata-only [--versions ...]`** ← current.
   Re-emit published `(version, platform)` manifests against the spec's current metadata,
   reusing existing layer digests. Plus metadata-drift detection in `pipeline plan`.
   Motivating case: the `binaries` field landed `b624a004` (2026-07-20), after the pilots
   published, so their published metadata predates it.
2. **`ocx-mirror#16` — buildtools shape.** Upstream ships 24 raw binaries (buildifier,
   buildozer, unused-deps × 8 platforms; no archive). Either three separate specs (zero code)
   or a `mirror.yml` multi-asset-per-platform shape (new feature).
3. **Second pilot** — `astral-sh/python-build-standalone`, exercising the same two
   capabilities against a large platform matrix.

## Reading the docs

The announce-from-registry doc updates are **on `main`** (`0df96e4`). Read them before writing
anything for #9, so its docs match the shape just set:

```
git show --stat 0df96e4 -- docs/
  docs/reference/cli.md          +25/-3    the announce verb and its flags
  docs/reference/environment.md  +26/-0    OCX_ANNOUNCE_TOKEN
  docs/reference/mirror-yml.md   +21/-7    catching up an existing mirror
```

Render locally rather than reading raw Markdown — nav and link breakage only shows in the build:

| Repo | Command | Notes |
|---|---|---|
| `ocx-mirror` | `task docs:serve` | mkdocs, http://127.0.0.1:8000 |
| `ocx` | `task website:serve` | VitePress |

`task docs:build` is the strict variant (broken nav or links fail the build); `task docs:lint:links`
checks external links. Run the strict build before claiming a doc change is done.

## Settled for #9 (owner, 2026-07-28)

- **Range is CLI-only.** `--min-version` (inclusive) / `--max-version` (exclusive), matching
  the `platforms:` vocabulary already in the spec (`src/spec.rs:290-295`). Both optional —
  omit one for an open end, omit both for every published version. The existing exact-list
  `--version` stays.
- **No `min_version_mode` inclusive/exclusive knob.** YAGNI; one convention, stated once.
- **No dash range syntax** (`1.2-2.0`). `-` is already the prerelease separator *and* the
  variant separator in the version grammar (`ocx crates/ocx_lib/src/package/version.rs:413,423`),
  so a dash range is genuinely ambiguous — this is a grammar conflict, not a style preference.
- **No range in `mirror.yml`.** A stored range means CI re-evaluates it every push, which drags
  in a patch ledger: which range maps to which metadata revision, has-this-been-patched, and
  when a range retires. Avoided entirely by the drift check below.
- **Idempotency via the config-blob digest, not a ledger.** Metadata lives in the config blob,
  not a layer, so comparing the published config-blob digest against the digest of what the
  current spec would produce is exact and cheap. Equal → already patched, skip. This is #9's
  `pipeline plan` drift-detection half; with it, `plan` reports drift and a human dispatches.
- **A patch must chain into announce.** Patching re-emits manifests, so their digests change
  and the index root's tag pointers go stale — a patched mirror whose root still points at the
  old digests is worse than an unpatched one. `pipeline patch` therefore ends by invoking the
  same announce path `pipeline announce` uses (`0df96e4`, already on main). Reuse it; do not
  add a second announce path.
- **No build timestamp.** Each patch generation produces a new manifest and therefore its own
  canonical `sha256.<hex>` tag, which is the audit trail. Old manifests stay reachable by
  digest, so pinned `@sha256:` consumers are unaffected. Verify during #9 that the prior
  canonical tag is not evicted.

## Open decisions (owner)

- **buildtools shape**: three specs vs. multi-asset `mirror.yml`. Recommendation: three specs
  — layer reuse buys nothing across three unrelated static binaries.
