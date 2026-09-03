# hex — swarm memory

Maintained by the hex skills. Small by contract: pointers and
preferences, not copies. Team-shared — commit it.

## Pointers

- Verification: `CLAUDE.md` › "Build & Development" — `task` (fast check),
  `task verify` (full gate), `task rust:verify` (Rust-only loop gate),
  `task test:parallel` (acceptance).
- Spec / plan / ADR conventions: planning flow ADR → Design Spec → Plan →
  Implementation; templates in `.claude/templates/artifacts/`, durable
  artifacts (ADRs, design specs) in `.claude/artifacts/`, executable plans +
  status tracking in `.claude/state/plans/`
  (`.claude/rules/meta-plan-status.md`). Shipped hex templates are the
  fallback only.
- Product knowledge: `docs/index.md` (overview) + `CLAUDE.md` › "Product"
  (users, comparable tools, research keywords).
- Key rules: `.claude/rules/subsystem-mirror.md` (module map, pipeline
  phases, spec format, error model); `CLAUDE.md` › "Dependency model" —
  `external/ocx` submodule is read-only, `[patch.crates-io]` table must
  never be dropped.
- Worktrees: default `.agents/worktrees/` (gitignored).
- Constitution: none.
- Federation: `ocx` → `../ocx` (`https://github.com/ocx-sh/ocx.git`); verification documented in its `CLAUDE.md` › "Build & Development" — `task verify`. Satellite for plan `mirror-signing`; the vendored `external/ocx` submodule stays read-only and is consumed by pointer bump.
- Discussions: `.agents/discussions/<slug>.md` (hex-discuss artifacts; `State:` header is the hex-state signal).

## Preferences

```yaml
# hex config, vocabulary v2. Unknown keys warn once and are ignored.
# (v1 = these keys minus `workflows`; see Key vocabulary.)
models:
  fast-balanced: sonnet
  deep-reasoning: opus
adversary: codex:codex-rescue
research-axes:
  - registry ecosystems
  - OCI spec evolution
  - package-manager supply chain
```

## Memory

- Learned: the acceptance suite talks to `localhost:5000`, and `test/docker-compose.yml`
  declares no project `name:`, so it defaults to `test` — the same default a sibling
  ocx repo's compose uses. When one of those is up, `task test:quick` silently reuses
  *its* container (zot, not `registry:2`) and `test_patch_evicts_nothing_a_consumer_could_have_pinned`
  fails on a manifest zot drops. Fixed 2026-08-03: `test/docker-compose.yml` now
  pins `name: ocx-mirror-test` and maps the registry to host port 5001, and the
  conftest `REGISTRY` default is `localhost:5001` — no collision surface left.
- Learned: `task test:quick` is the acceptance loop that skips the rebuild —
  there is no `--no-build` pytest flag.
- Discussion hand-off 2026-09-02: `.agents/discussions/mirror-signing.md`
  → plan (`handed-off → plan`). Decisions: mirror signs its own pushes
  (keyless default, `--key` schemes as fallback), copies preserve upstream
  signatures and carry the whole referrer graph + verbatim sidecars,
  fallback-index merge in ocx's D4 shape, fail closed, backfill pre-filtered
  by identity, federated plan (lead `.` + `ocx` satellite at `../ocx`).
  Research index: `.claude/artifacts/research_mirror_signature_carriage.md`,
  `research_oci_signing_sota_2026.md`, `research_mirror_signing_archaeology.md`,
  `research_mirror_signing_recon.md`, `research_relocation_verify.md`.
- Active plan 2026-09-02: `.claude/state/plans/plan_mirror_signing.md`
  (tier high, federated: lead `.` + satellite `ocx` at `../ocx`, shared slug
  `hex/mirror-signing`). ADR `.claude/artifacts/adr_mirror_signing.md`.
  12 WPs / 6 waves; critical path WP 7 → WP 9 → WP 2 → WP 4 → WP 11 → WP 12.
  D1 amended by owner ruling 2026-09-02 (Option 3 → Option 4: `sign:` carries a
  `keyless` xor `key` mode tag, one `Ref` grammar, publish-side Fulcio/Rekor
  always emitted, `format` dropped). That added OCX-C-5 (`push --sign` gains
  `--fulcio-url`/`--rekor-url`), which puts the push leg behind the satellite —
  WP 2 moved to wave 3 and `ocx.toml`'s `ocx` pin no longer stays at 0.6.0.
  Owner gates at execute time: confirm `e2e-ocxmirror-signing` repo creation
  (WP 12 tier 2); push ocx `hex/mirror-signing` before WP 9 merges; confirm
  ocx's stale `feat/signing-and-trust` is superseded; cut an ocx release
  carrying OCX-C-5 and bump `ocx.toml` off 0.6.0 before the mirror release
  (WP 12).
- Note for the next `/hex-init` re-audit: `meta-plan-status.md`'s `Step:`
  vocabulary is swarm-era (`/swarm-plan → …`); hex plans write
  `Step: /hex-plan → plan-approved` etc. Reconcile the vocabulary or exempt hex.
