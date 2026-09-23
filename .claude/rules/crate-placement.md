---
paths:
  - src/**
  - crates/**
  - Cargo.toml
  - external/ocx/**
---

# Crate Placement

Mirror-native. Where new code goes: mirror-owned vs. promoted to an ocx
ecosystem crate, and which of the mirror's own crates it lands in — the
phase 1 split (`adr_bazel_crate_split.md`) has landed. Detail and rationale
live in that ADR — this rule is the quick decision table, not a restatement
of it.

## Where new code goes

| Code is… | Goes in |
|---|---|
| A spec grammar (`mirror.yml`/`registry.yml`/`dist.yml` types, validation, `extends:` merge) | `ocx_mirror_spec` |
| Pipeline orchestration (prepare/push phases, `MirrorTask`, cascade, registry sync/copy) | `ocx_mirror_pipeline` |
| CI-workflow rendering (`pipeline generate ci`, templates, drift guard) | mirror root (`ocx_mirror`; `src/command/package/pipeline/generate/`) |
| `MirrorError` and its exit-code mapping, or a `From<Local> for MirrorError` | `ocx_mirror_error` |
| The `ocx` subprocess boundary (binary resolution, argv assembly, `OCX_*` env forwarding) | `ocx_mirror_pipeline::ocx_cli` |
| Format/protocol logic (parsing, wire types) with a **named second caller that has a real call site** | promote to an ocx ecosystem crate — see Promotion below |
| Generic logic with no dependency on `MirrorError` or a mirror spec type, not (yet) promoted | a generic `ocx_mirror_*` crate (`http`, `report`, `source`, `test_support`) |
| Application glue (CLI dispatch, `main.rs`, the `lib.rs` façade re-exports) | mirror root package (`ocx_mirror`) |

A spec grammar validates by calling into the module that owns the grammar —
`ocx_mirror_spec::registry` calls `ocx_mirror_spec::destination` and
`ocx_mirror_spec::glob` directly, because `destination`, `layout`, `glob` and
`catalog::index_host` moved *down* into `ocx_mirror_spec` per
`adr_bazel_crate_split.md` § C1 (they used to live in the pipeline module,
which would have made spec depend upward on pipeline). `ocx_mirror_pipeline`
depends on the spec crate for the ones it still needs — never the reverse. No
upward edges: a generic or lower-tier crate never depends on a crate above it
in the map.

## ocx crate tiers

Three tiers, defined in ocx's own `CLAUDE.md` and `adr_crate_split_workspace.md`:

- **Internal** — free to change, no announcement, no stability at all
  (`ocx_store`, `ocx_project`, `ocx_package_manager`, `ocx_shell`,
  `ocx_announce`, `ocx_script`, `ocx_setup`, `ocx_test_support`, `ocx_schema`).
- **Ecosystem** — a crate a lockstep submodule consumer (this mirror) links.
  Breaking changes are allowed when justified; ocx's own `task
  satellite:verify` obligates it to upgrade the mirror in the same change
  series. `ocx_util`, `ocx_console`, `ocx_oci`, `ocx_trust`, `ocx_sign`,
  `ocx_config`, `ocx_index`, `ocx_package`.
- **Interface** — the CLI surface, every wire/persisted format, the shim wire
  ABI, and `ocx_exit` (an exit code is a CLI contract).
- `ocx_cli` is not a tier crate (application layer), and the satellite
  linking rule (below) forbids naming it.

## The satellite linking rule

A satellite (this mirror) may take a **direct** dependency on any
*ecosystem*-tier crate and on `ocx_exit` (interface tier) — and on no other
ocx crate. It must not name an internal-tier crate or `ocx_cli` in its
manifest or its source.

**The eight allowed today:** `ocx_config`, `ocx_console`, `ocx_exit`,
`ocx_index`, `ocx_oci`, `ocx_package`, `ocx_sign`, `ocx_util`. Plus
`ocx_python` from phase 2 of `adr_bazel_crate_split.md` (an owner-directed
exception — see Promotion below). CLAUDE.md's "Dependency model" section is
the authoritative row list; this rule states the boundary, not the count.

Two sanctioned transitive paths exist and do not widen the rule: `ocx_index`
and `ocx_package` both depend on `ocx_store` (internal tier), so linking
either compiles `ocx_store` without permission to name it directly.

`ocx_cli::` must never appear as a path root in mirror code — write
`crate::ocx_cli::`/`super::…` for the mirror's own subprocess-boundary
module (`ocx_mirror_pipeline::ocx_cli`, unrelated to ocx's crate of the same name).
ocx's satellite scan reads a bare `ocx_cli::` reference as the forbidden
crate regardless of which `ocx_cli` it resolves to.

## Promotion trigger

Format/protocol logic promotes out of the mirror into an ocx ecosystem crate
when a **named second caller with a real call site** exists, recorded in an
ADR. "Might be useful to ocx someday" is not a trigger — a live consumer is.

**`ocx_python`'s phase-2 promotion (`adr_bazel_crate_split.md` § Q3, § C5) is
an owner-directed exception, not precedent.** It moved on the owner naming
`ocx-dist` (planned, no repository yet) as the second caller, before that
caller exists. Do not cite it to justify promoting code with no live
consumer — this rule and any future ADR must treat it as a one-off grant,
not a lowered bar.

How to promote: the `/ocx-upstream-pr` skill authors the PR against
`external/ocx`; landing and pointer adoption are owner/`/update-ocx` actions
(`adr_bazel_crate_split.md` § C6).

## The mirror crate layout

**Landed — phase 1 of `adr_bazel_crate_split.md`; authority
`crates/crate_map.toml`.**

| Crate | Kind | Roughly |
|---|---|---|
| `ocx_mirror_test_support` | dev-only | Shared test env-lock helpers |
| `ocx_mirror_http` | generic | HTTP client construction, auth, retry jitter |
| `ocx_mirror_report` | generic | JUnit parsing, run summary, Discord webhook |
| `ocx_mirror_source` | generic | Upstream source discovery (github_release, url_index, pypi, pylock adapter), resolver |
| `ocx_mirror_error` | mirror | `MirrorError`, exit codes, `From<Local>` conversions |
| `ocx_mirror_spec` | mirror | `MirrorSpec` and every spec grammar, filter, normalizer |
| `ocx_mirror_pipeline` | mirror | Prepare/push orchestration, `ocx` subprocess boundary |
| `ocx_mirror` (root) | app | CLI dispatch, `main.rs`, façade re-exports, CI-workflow rendering |

`ocx_python` is a workspace member in phase 1 (`crates/ocx_python`, mirror-owned)
and an `external/ocx` path crate from phase 2 on.

## Rules for new code

- **Naming:** every mirror-owned crate is `ocx_mirror_*`.
- **No upward edges.** A generic crate never depends on a mirror crate. If a
  type a generic crate needs turns out to be mirror-shaped, move the type
  *down* into the generic crate and re-export it at the old path — never add
  the opposite edge.
- **Generic crates carry no `MirrorError` and no spec type** —
  `adr_bazel_crate_split.md` § C3, quoted exactly: **E1** `ocx_mirror_http`
  (`http`, `auth`), `ocx_mirror_report` and, in `ocx_mirror_source`,
  `github_release::{client, api_client}` return local `thiserror` enums; no
  generic crate names `MirrorError` or a spec type. **E5** `anyhow` stays in
  the `ocx_mirror_source` API; `thiserror` conversion is promotion debt. Conversion to
  `MirrorError` lives in `ocx_mirror_error` alone.
- `pub(crate)` widens to `pub` only for items that actually cross a crate
  boundary. No cross-crate `pub use *`.

## See also

- `.claude/artifacts/adr_bazel_crate_split.md` — § C1 (crate map, full
  contract), § C3 (error boundary), § Q3 (the `ocx_python` promotion
  decision), § C9(a) (this rule's own design record).
- `CLAUDE.md` § "Dependency model" — the authoritative eight-row list and why
  `ocx_cli` never gets a row.
- `.claude/rules/subsystem-mirror.md` — the module map, keyed to the crate layout.
