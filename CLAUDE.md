# CLAUDE.md

Guide Claude Code (claude.ai/code) in this repo.

## What is ocx-mirror

Standalone Rust CLI (`ocx-mirror`) that mirrors upstream tool releases
(GitHub Releases, URL indexes) into OCI registries as
[OCX](https://github.com/ocx-sh/ocx) packages. Split out of the ocx mono-repo;
same authors, same conventions. Architecture rule:
[.claude/rules/subsystem-mirror.md](./.claude/rules/subsystem-mirror.md)
(module map, pipeline phases, spec format, error model).

## Layout

| Path | Purpose |
|------|---------|
| `src/` | The crate (binary `ocx-mirror`), package manifest at repo root |
| `external/ocx` | **git submodule** — vendored ocx; `ocx_lib` is a path dep into it |
| `tests/fixtures/` | Renderer/spec fixtures for unit tests |
| `test/` | pytest acceptance harness (Docker registry on :5000) |
| `docs/` + `mkdocs.yml` | mkdocs-material site → GitHub Pages |
| `packaging/metadata.json` | OCX package metadata used by publish workflows |
| `CATALOG.md` + `assets/logo.svg` | Registry catalog description — pushed via `ocx package describe` in `oci-publish.yml` (frontmatter = title/description/keywords) |
| `src/command/pipeline/generate/templates/` | Workflow templates baked into the binary (Renovate customManager bumps their action pins) |

## Dependency model (read before touching Cargo.toml)

- `ocx_lib = { path = "external/ocx/crates/ocx_lib" }` — NOT a published crate.
  Bumping ocx_lib = bumping the submodule pointer (procedure in README.md).
- `[patch.crates-io]` re-declares ocx's fork patches pointing into the
  **nested** submodules (`external/ocx/external/...`). Patches do not travel
  with path deps; dropping the table silently resolves unpatched crates.io
  releases. CI asserts the fork source via `cargo tree -i oci-client`.
- Dependency feature lists for deps shared with `ocx_lib`/`ocx_cli` are copied
  exactly from ocx's `[workspace.dependencies]` — keep in sync on submodule
  bumps. **Since v0.4.1** (the upstream commit that extracted ocx-mirror)
  `reqwest`, `rustls`, `octocrab`, `url` are mirror-owned — ocx dropped them, so
  there is no upstream source of truth for these four.
- Clone/checkout always `--recurse-submodules`.

## Build & Development

Task runner [`task`](https://taskfile.dev). `task` (fast check),
`task verify` (full gate), `task rust:verify` (Rust-only loop gate),
`task test:parallel` (acceptance), `task docs:serve`. Toolchain via direnv +
`ocx direnv export` (`ocx.toml`). Always `cargo fmt` before commit,
`task verify` after implementation.

Single acceptance test:

```sh
cd test && uv run pytest tests/test_mirror.py::<name> -v --no-build
```

## Registries

| Channel | Target | Trigger |
|---------|--------|---------|
| Dev | `dev.ocx.sh/ocx/mirror:<ver>-dev_<TS>` + cascade | manual `Deploy Dev` workflow |
| Release | `ocx.sh/ocx/mirror:<ver>_<TS>` + cascade | tag push `vX.Y.Z` |

## Workflow

Commits: [Conventional Commits](https://www.conventionalcommits.org/)
(`feat:`, `fix:`, `refactor:`, `ci:`, `chore:`). No `Co-Authored-By` trailers.
Work on branches, never `main`. **Never push** — human decides.
`task checkpoint` amends a rolling "Checkpoint" commit during work.

Releases: `task release:prepare` → human reviews → commit + tag + push
(see README.md).

Rules in `.claude/rules/` auto-load by path (`quality-rust`, `quality-core`,
`quality-python`, `subsystem-mirror`, `workflow-*`,
`meta-plan-status`, `meta-ai-config`). Design records live in
`.claude/artifacts/` (ADRs and design specs moved from the ocx mono-repo).

## Skills & Workflow

Every task starts with
[workflow-intent.md](./.claude/rules/workflow-intent.md) — classify
(feature/bugfix/refactor), check GitHub for related issues/PRs, route to
`workflow-feature.md` / `workflow-bugfix.md` / `workflow-refactor.md`.

Skills in `.claude/skills/` (ported from ocx): `/architect`,
`/swarm-plan`, `/swarm-execute`, `/swarm-review`, `/commit`, `/finalize`.
Worker agents the swarm skills spawn live in `.claude/agents/`.

Planning flow: ADR → Design Spec → Plan → Implementation. Templates →
`.claude/templates/artifacts/`; durable artifacts → `.claude/artifacts/`;
executable plans + status tracking → `.claude/state/plans/`
([meta-plan-status.md](./.claude/rules/meta-plan-status.md)).

Dev cycle: `/commit` (working phase, rolling Checkpoints) →
`/finalize` (clean conventional commits, fast-forward onto main). Full
model → [workflow-git.md](./.claude/rules/workflow-git.md).

> The ported AI-config surface (skills, agents, most rules) is plain copies
> from ocx; mirror-native files (`subsystem-mirror.md`, `meta-plan-status.md`,
> `meta-ai-config.md`, `.claude/artifacts/`) are owned here.
> [meta-ai-config.md](./.claude/rules/meta-ai-config.md) governs the port /
> re-sync protocol and the adaptation list — register any new port in this file
> the same commit it lands. Grimoire-package distribution is a planned
> follow-up; until then keep ported edits upstream-compatible.
