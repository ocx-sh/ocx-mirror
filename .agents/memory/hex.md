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
  `external/ocx` submodule is read-only (except under `/ocx-upstream-pr` on a
  submodule feature branch), `[patch.crates-io]` table must never be dropped.
- Worktrees: default `.agents/worktrees/` (gitignored).
- Constitution: none.
- Federation: `ocx` → `../ocx` (`https://github.com/ocx-sh/ocx.git`); verification documented in its `CLAUDE.md` › "Build & Development" — `task verify`. Satellite for plan `mirror-signing`; the vendored `external/ocx` submodule stays read-only (except under `/ocx-upstream-pr` on a submodule feature branch) and is consumed by pointer bump.
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
- Discussion hand-off 2026-09-22: `.agents/discussions/bazel-crate-split.md`
  → architect (`handed-off → architect`), consumed by an autonomous `/goal` (prompt in the
  artifact's `## Goal prompt`; run ledger `.agents/goal/bazel-crate-split.md`). Decisions: phases
  AI config → crate split → promote `ocx_python` to ocx (submodule-authored PR, run squash-merges
  — superseded by ADR A-2/C6.3: landing is an owner action)
  → Bazel (local loop mandatory, CI swap only on a pre-declared measured bar); oracle = `test/` at
  `v0.6.2` unmodified via `OCX_MIRROR_COMMAND`. Research:
  `.claude/artifacts/research_bazel_crate_split_lessons.md`.
- Umbrella ADR 2026-09-22: `.claude/artifacts/adr_bazel_crate_split.md` (Proposed) closes the
  bazel-crate-split dossier Q1–Q5. Research: `research_bazel_cross_module_ocx.md`,
  `research_mirror_crate_graph.md`. Learned: ocx-sh/ocx allows rebase-merge only (squash off) with
  required_signatures — the run may not squash; landing is an owner action (ADR C6.3). Codex
  adversary was out of usage that day. Axis worth a Preferences hint: "Bazel / crate_universe".
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
- Plan 2026-09-23 done: `.claude/state/plans/plan_bazel_phase1_crate_split.md` (crate split, Loop 1 of
  the bazel-crate-split goal). Learned: in a crate split the review perspective that mattered was
  test-coverage — moved tests kept their names (count gate green) but lost end-to-end assertions;
  cross-crate `#[cfg(test)]` shortcuts need an inventory before the first move. rtk-proxied `diff`
  reported differing schema files identical — use `/usr/bin/diff`.
- Plan 2026-09-23 done: `.claude/state/plans/plan_bazel_phase2_ocx_python.md` (Loop 2 of the
  bazel-crate-split goal: `ocx_python` promoted as ocx-sh/ocx#503, pointer on the verified PR head).
  Learned: ocx's commit-gate hook blocks local submodule commits without a verify mark — scratch commits
  take `--no-verify`, the full ocx `task verify` is the gate. Mutation-think in review caught a
  prefix-match `find_package` that passed every test — ask "which wrong implementation survives?".
  Learned at plan review: ocx registers a crate in far more places than its ADR lists
  (`bazel_gate_proofs.py` counts, `bazel_label_map.toml`, `scoped_gate.py` ECOSYSTEM,
  `NOT_ON_THE_BOUNDARY`, crate README tier line, LICENSE-THIRD-PARTY) — grep ocx for an
  existing ecosystem crate's name before trusting a registration list.
