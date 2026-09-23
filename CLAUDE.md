# CLAUDE.md

Guide Claude Code (claude.ai/code) in this repo.

## What is ocx-mirror

Standalone Rust CLI (`ocx-mirror`) that mirrors upstream tool releases
(GitHub Releases, URL indexes) into OCI registries as
[OCX](https://github.com/ocx-sh/ocx) packages. Split out of the ocx mono-repo;
same authors, same conventions. Architecture rule:
[.claude/rules/subsystem-mirror.md](./.claude/rules/subsystem-mirror.md)
(module map, pipeline phases, spec format, error model).

## Product

Overview: [docs/index.md](./docs/index.md) — what it does, how it works.
Users: repo operators publishing tool mirrors; runs locally and in GitHub Actions.
Comparable tools: aqua registry, mise/asdf backends, ubi, `crane`/`oras` (raw OCI copy).
Research keywords: OCI artifacts, ORAS, GitHub Releases API, cascade tags, registry mirroring.

Principle: every ocx-mirror capability is reachable as **one command** an operator
can paste into any CI system; rendered pipelines are a GitHub-only convenience,
never the only path. Corporate users run GitLab, Jenkins, or a cron box, and
their runners, proxies, and auth are not ours to model — ship the command,
document the four-line job, let them own the pipeline.

## Layout

| Path | Purpose |
|------|---------|
| `src/` | The root crate (binary `ocx-mirror`): CLI dispatch (`command/`), `main.rs`, the `lib.rs` façade; package manifest at repo root |
| `crates/ocx_mirror_*` | Cargo workspace members from the phase-1 crate split (`error`, `http`, `pipeline`, `report`, `source`, `spec`, `test_support`); each `Cargo.toml` inherits `[workspace.dependencies]` via `.workspace = true` |
| `crates/crate_map.toml` | Authority for which crate may depend on which; enforced by `tests/workspace_structure.rs` |
| `external/ocx` | **git submodule** — vendored ocx; its `ocx_*` crates are path deps into it |
| `tests/workspace_structure.rs` | Reads `cargo metadata`; fails naming the offender on any crate-map violation (upward edge, unlisted `ocx_*`, missing `[lints] workspace = true`, …) — the enforcement half of `crates/crate_map.toml` |
| `tests/source_scan.rs` | Cross-crate source scans that need the whole tree at once: the extra-roots self-scan (walks `src/` and every `crates/*/src/`) and the cross-crate `include_str!` factory scan |
| `tests/fixtures/` | Renderer/spec fixtures for unit tests |
| `MODULE.bazel` (+ `.lock`), `.bazelrc`, `.bazelversion` | Bazel module (Linux dev loop only; release and non-Linux stay cargo). Third-party crates come from `Cargo.toml`/`Cargo.lock` via `crate.from_cargo`; the generated `Cargo.bazel.lock.json` is gitignored |
| `BUILD.bazel`, `crates/*/BUILD.bazel`, `test/BUILD.bazel` | Hand-written Bazel packages (root lib/bin + tests, the seven crates, the acceptance suite as one `sh_test`); `bazel:build:drift` keeps their edges equal to Cargo's |
| `crates/TEST_TARGET_MAP.toml` | Per-target Bazel test counts (rise only) + `[[excluded]]` cases Bazel skips and nextest runs |
| `scripts/` | Gate tooling: `bazel_test_floor.py`, `bazel_build_drift.py`, `bazel_cache_check.py`, `bep_to_otlp.py`, `bazel_scoped.py` (each has `--self-test`; `task scripts:self-test` runs all five) |
| `scripts/test_telemetry_names.py` | pytest (not `--self-test`) for the telemetry names + never-fail contract; `task telemetry:self-test` |
| `test/bazel_accept.sh` | The `//test:acceptance` `sh_test` runner: points the harness at the Bazel-built `ocx-mirror` and the pinned `ocx`, then `pytest -n auto` |
| `.github/actions/test-telemetry/` | Composite action pushing a JUnit report's timings to otel.ocx.sh from CI (the `push` task's CI half) |
| `test/` | pytest acceptance harness (Docker registry on :5001) |
| `docs/` + `mkdocs.yml` | mkdocs-material site → GitHub Pages |
| `packaging/metadata.json` | OCX package metadata used by publish workflows |
| `CATALOG.md` + `assets/logo.svg` | Registry catalog description — pushed via `ocx package description push` in `oci-publish.yml` (frontmatter = title/description/keywords) |
| `src/command/package/pipeline/generate/templates/` | Workflow templates baked into the binary (Renovate customManager bumps their action pins) |

## Dependency model (read before touching Cargo.toml)

- Nine `ocx_*` path rows into `external/ocx/crates/` — NOT published crates.
  They live in root `[workspace.dependencies]` once, and every member that
  needs one takes it `.workspace = true`. `ocx_python` (PEP 751 lock → OCX
  package translation) is one of them; its git-pinned `uv-*` rows live in
  ocx's `[workspace.dependencies]` and move with the pointer. Bumping ocx
  = bumping the submodule pointer (procedure in README.md); the row list
  changes only when the code names a new crate.
- **Only ecosystem- or interface-tier crates may get a row.** ocx's `task
  satellite:verify` (a `verify-deep.yml` job) asserts this workspace resolves
  no internal-tier crate: internal code carries no stability at all, so a link
  is a break waiting for the next rename. `ocx_shell` and `ocx_announce` were
  dropped for this reason ([ocx-sh/ocx#497](https://github.com/ocx-sh/ocx/issues/497));
  what each was doing is now `--ci-annotations` on the push argv and
  `crates/ocx_mirror_spec/src/forge.rs` respectively.
- **Never add a row for `ocx` itself** (`external/ocx/crates/ocx_cli`). It is an
  application, not a library with an interface, and linking it for two imports
  cost 160 packages — the whole Starlark host, an LSP/DAP/REPL stack and the gix
  family. The two things the mirror wanted from it are mirror-owned now:
  `src/tracing_init.rs` (a verbatim copy of ocx's, feature-tracked through the
  `tracing-subscriber` row) and `ocx_mirror_error::tls_exit_code` (a copy of ocx's
  `impl ClassifyExitCode for TlsError`, guarded by an exhaustive match and the
  `tls_error_codes_match_ocx` unit test). Both are deleted upstream-side only by
  the `ocx_tracing` extraction named in `src/tracing_init.rs`.
- `[patch.crates-io]` re-declares ocx's fork patches pointing into the
  **nested** submodules (`external/ocx/external/...`). Patches do not travel
  with path deps; dropping the table silently resolves unpatched crates.io
  releases. CI asserts the fork source via `cargo tree -i oci-client`;
  `task bazel:patch:check` asserts the same fork binding under Bazel (in
  `Cargo.bazel.lock.json`).
- Dependency feature lists for deps shared with ocx are copied
  exactly from ocx's `[workspace.dependencies]` — keep in sync on submodule
  bumps. `octocrab` is mirror-owned outright — no ocx equivalent exists to sync
  against. `url` **was** mirror-owned; **since v0.6.0** ocx declares it too
  (`url = { version = "2.5.8", features = ["serde"] }`), the mirror already
  matches it byte-for-byte, and it now belongs in the copy-exactly set.
  `rustls` is mirror-owned too: ocx only pulls it as a
  feature of its own `reqwest` dependency, never as a bare top-level
  dependency, so there is nothing to copy. **Since v0.5.8** `reqwest` is back
  in ocx's `[workspace.dependencies]` (`ocx_oci` depends on it directly), and
  ocx-mirror **tracks its major** — `0.13`, `rustls`, plus `json` which is
  mirror-owned. Keep them on one major: a split major linked two copies of the
  crate and made `ocx_oci`'s reqwest types unnameable here, which is how the
  mirror ended up with its own TLS-root handling and a corporate CA that no leg
  trusted. `crates/ocx_mirror_http/src/lib.rs` now calls `ocx_util::tls::seed_embedded_roots`
  directly. **Since v0.6.1** ocx adds `system-proxy` (its SSRF guard consults
  reqwest's own proxy matcher), so the mirror carries it too — copy-exactly.
- Clone/checkout always `--recurse-submodules`.

## Build & Development

Task runner [`task`](https://taskfile.dev). `task` (fast check),
`task verify` (full gate), `task rust:verify` (Rust-only loop gate),
`task test:parallel` (acceptance), `task docs:serve`. Toolchain via direnv +
`ocx direnv export` (`ocx.toml`). Always `cargo fmt` before commit,
`task verify` after implementation.

Single acceptance test:

```sh
cd test && uv run pytest tests/test_mirror.py::<name> -v
```

**Bazel loop (Linux).** `task rust:test:unit` and `task rust:verify` run
`bazel:test:unit` on Linux (nextest elsewhere; CI stays nextest). Bazel runs
via `ocx exec bazel -- bazel`. `task bazel:bootstrap` first in a fresh
worktree (generates `Cargo.bazel.lock.json`, restores `external/ocx`, writes
`test/acceptance.stamp`); `bazel:test:unit`, `bazel:test:accept`
(acceptance, cached — same env as `test:parallel`), `bazel:test:scoped`
(rdeps of what changed vs `origin/main`), `bazel:cache:gc` (manual,
`MAX_GB=30`). A manual repin needs
`--repo_env=TMPDIR=/var/tmp/ocx-mirror-splice` on hosts with a
`~/.cargo/config.toml` above `/tmp`. RAM: `.bazelrc` caps the JVM at 2 GB and
actions at 30% of host RAM, tasks pass `--jobs=6` (`JOBS=` overrides); keep
one Bazel server per worktree and `bazel shutdown` when idle. Telemetry to
otel.ocx.sh is off unless `~/.config/ocx-telemetry/env` names an endpoint
(`taskfiles/telemetry.taskfile.yml` header).

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

Rules in `.claude/rules/` auto-load by path (`quality-core`, `quality-rust`,
`quality-rust-errors`, `quality-rust-exit_codes`, `quality-python`,
`python-packaging`, `subsystem-mirror`, `crate-placement`,
`security-threat-model`, `workflow-*`, `meta-plan-status`, `meta-ai-config`,
and the `rust-quality`, `rust-cargo`, `python-quality`, `docs-quality`,
`bazel-quality` bundles). Design records live in
`.claude/artifacts/` (ADRs and design specs moved from the ocx mono-repo).

**Every security review reads
[security-threat-model.md](./.claude/rules/security-threat-model.md) first** — it
defines who this project defends against (outside attackers; the execution
environment is trusted, by owner ruling). A finding outside that boundary is not
a finding. This applies to human review, `/security-review`, any reviewer agent
in `security` focus, and any cross-model adversary pass.

## Skills & Workflow

Every task starts with
[workflow-intent.md](./.claude/rules/workflow-intent.md) — classify
(feature/bugfix/refactor), check GitHub for related issues/PRs, route to
`workflow-feature.md` / `workflow-bugfix.md` / `workflow-refactor.md`.

Skills in `.claude/skills/` (ported from ocx): `/architect`,
`/swarm-plan`, `/swarm-execute`, `/swarm-review`, `/commit`, `/finalize`.
Mirror-native: `/e2e-test` (tiered e2e: acceptance harness → local contrib
integration → dev.ocx.sh dev channel), `/update-ocx` (bump the `external/ocx`
submodule and adopt what changed: drift gates, semantic review of the nine
linked crates, consolidation onto new shared API, upstream-issue cross-check),
`/ocx-upstream-pr` (author an ocx-sh/ocx PR from the `external/ocx` submodule;
landing is an owner action). Worker agents the swarm skills spawn live in
`.claude/agents/`.

`.claude/hooks/pre_tool_use_guard.py` is a `PreToolUse` guard (rules G1–G5)
blocking git/edits inside `external/ocx` on `main`/detached HEAD or without an
absolute `-C`, a root-level recursive `submodule update`, and writes to the
sibling `../ocx`; tested via `task claude:hooks:test`, part of `task verify`.

Planning flow: ADR → Design Spec → Plan → Implementation. Templates →
`.claude/templates/artifacts/`; durable artifacts → `.claude/artifacts/`;
executable plans + status tracking → `.claude/state/plans/`
([meta-plan-status.md](./.claude/rules/meta-plan-status.md)).

Dev cycle: `/commit` (working phase, rolling Checkpoints) →
`/finalize` (clean conventional commits, fast-forward onto main). Full
model → [workflow-git.md](./.claude/rules/workflow-git.md).

> The ported AI-config surface (skills, agents, most rules) is plain copies
> from ocx; mirror-native files (`subsystem-mirror.md`, `crate-placement.md`,
> `security-threat-model.md`, `meta-plan-status.md`, `meta-ai-config.md`,
> `skills/{e2e-test,update-ocx,ocx-upstream-pr}/`, `hooks/`,
> `.claude/artifacts/`) are owned here.
> [meta-ai-config.md](./.claude/rules/meta-ai-config.md) governs the port /
> re-sync protocol and the adaptation list — register any new port in this file
> the same commit it lands. Grimoire-package distribution of the ocx-ported
> surface is a planned follow-up; the lore bundles in `grimoire.toml` are
> already grimoire-managed.
