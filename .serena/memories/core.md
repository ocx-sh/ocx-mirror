# Core

Standalone Rust CLI `ocx-mirror` (crate `ocx_mirror`, bin `ocx-mirror`): mirrors upstream tool releases (GitHub Releases, URL indexes) into OCI registries as OCX packages. Split out of the `ocx` mono-repo; same authors/conventions.

## Authoritative docs (read these, don't duplicate here)
- `CLAUDE.md` (repo root) — layout table, dependency model, registries, workflow entry.
- `.claude/rules/subsystem-mirror.md` — full module map, two-phase pipeline, spec format, `MirrorError` exit-code table, test-pipeline subcommand contracts. Path-scoped (auto-loads on `src/**`,`tests/**`).
- `.claude/rules/` auto-load by path: `quality-core`, `quality-rust`(+`-errors`,`-exit_codes`), `quality-python`, `workflow-{intent,feature,bugfix,refactor,git,swarm}`, `meta-{plan-status,ai-config}`.

## Source map (src/, single crate, manifest at repo root)
- Commands: top-level `command.rs` dispatches `package` (group) + `schema`. Package verbs `command/package/{sync,check,validate,target_registry,options,mod}.rs`; `command/package/pipeline/{plan,prepare,push,notify,mod}.rs` + `generate/` (workflow renderer, templates baked in). `command/registry/` is a reserved placeholder (no live verb yet — see `adr_cli_namespace_restructure`).
- Pipeline engine: `pipeline/{orchestrator,download,verify,package,push,mirror_task,mirror_result}.rs`; shared helpers `pipeline.rs`.
- Spec config types: `spec/` (root `spec/spec.rs` `MirrorSpec`+`load_spec`; one file per config block).
- Sources: `source/{github_release,url_index}.rs`. Stages: `normalizer.rs`,`resolver.rs`,`filter.rs`. Reporting: `junit.rs`,`run_summary.rs`,`annotations.rs`,`discord.rs`,`version_platform_map.rs`. Errors: `error.rs`.

## Project-wide invariants
- Two-phase pipeline: prepare (concurrent) → push (sequential, oldest-first) so cascade tags (X.Y.Z→X.Y→X→latest) push in semver order. Never reorder.
- Per-(version,platform) applicability is two `MirrorSpec` predicates: `platform_applies` + `exclude_hit` (single source of truth). Don't re-implement filtering ad hoc.
- Generated workflows are a drift-guarded surface (`package pipeline generate ci --check` exits 65). Edit templates in `command/package/pipeline/generate/templates/`, not output.

See also: `mem:tech_stack`, `mem:suggested_commands`, `mem:conventions`, `mem:task_completion`.