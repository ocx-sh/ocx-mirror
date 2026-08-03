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
  fails on a manifest zot drops. Run against an isolated `registry:2`
  (`REGISTRY=localhost:5001 task test:quick`) or pin a `name:` in the compose file.
- Learned: `task test:quick` is the acceptance loop that skips the rebuild —
  there is no `--no-build` pytest flag.
