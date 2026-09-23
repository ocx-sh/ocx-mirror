# Goal ledger — bazel-crate-split

Single source of run state. Re-read on every wake-up and after every compaction;
update before and after every sub-orchestrator spawn.

- Artifact: `.agents/discussions/bazel-crate-split.md` (ratified 2026-09-22 → architect)
- Research: `.claude/artifacts/research_bazel_crate_split_lessons.md`
- Mirror branch: `hex/bazel-crate-split` (from local `main` 537d591; origin/main 02481a5)
- Mirror branch pushed 2026-09-23, finalized + force-pushed (series 537d591..621310c + ledger); Mirror PR: [ocx-sh/ocx-mirror#89](https://github.com/ocx-sh/ocx-mirror/pull/89) — open, not merged (owner)
- ocx branch (in `external/ocx`): `feat/ocx-python-crate` (pushed; `delete_branch_on_merge` false)
- ocx PR: [ocx-sh/ocx#503](https://github.com/ocx-sh/ocx/pull/503) — MERGED by owner 2026-09-23T14:56Z as `15946973` on ocx main (parent `f38d22f6`, tree identical to PR head `77ae3cc0`; unsigned — landed via the merge button, not the FF)
- ocx merged SHA (pointer target): `159469736d6bc5a1f3d65ecf7c98a4cbac0bad9c` — pointer moved from PR head `77ae3cc0` 2026-09-23 (same tree)
- Submodule pointer at start: `external/ocx` 191b9324
- Drafts / long-job logs: `.tmp/` (gitignored, delete at end)

## Phase status

| Phase | Status | Loop | Gate evidence |
|---|---|---|---|
| U — umbrella `/hex-architect high` | done | umbrella-architect | ADR `.claude/artifacts/adr_bazel_crate_split.md` @ b2f974ea (Proposed; spec + adversarial opus review, 36 findings applied) |
| M — meta-plan of inner loops | done | meta | see § Meta-plan |
| 0 — AI config (placement rule, submodule-PR skill, guard hooks) | done | phase0-loop | commits bb16100..cb190dd (7); `claude:hooks:test` 104 passed; `ocx run -- task verify` EXIT=0 (`REGISTRY=localhost:5021`; `rust:test:unit` task-cached up to date — no Rust source changed); oracle baseline 202 pass / 0 fail / 2 skip; plan `.claude/state/plans/plan_bazel_phase0_ai_config.md` (local) |
| 1 — crate split (Cargo workspace, behaviour-preserving) | done | phase1-loop | commits 61eafc4..7bfd2b8 (bump, spike ADR A-4, 7 crates, review fixes, ADR A-6); `ocx run -- task verify` EXIT=0 at e21ba80 (`.tmp/verify-p1-final.log`, `REGISTRY=localhost:5021`; later commits a806c08/7bfd2b8 are test-comment/doc only, clippy+nextest 1633/1633 re-run green); gates `.tmp/fix2-gates.log`: G-count 1633 = 1599 + 34 added (base leaf names ⊆ new), G-features/G-schema/G-cli byte-identical to pre-split 9505fde, G-rustdoc 13 ≤ 15, G-patch forks, satellite scan + ocx's `satellite:verify` block green; oracle 202/0/2 = baseline; e2e tier 1 (differential vs pre-split binary) PASS, tier 2 blocked (needs push); /hex-review high Needs Work → fixed → delta round Approve; plan `.claude/state/plans/plan_bazel_phase1_crate_split.md` (local) |
| 2 — promote `ocx_python` + pylock into ocx (submodule PR, squash-merge, pointer bump) | done | phase2-loop | mirror commits 5a046ae..8e62543 (D-Q3 doc, 6 characterization tests, switch 9b36490, review fixes ec89800/87bafc9, ADR A-7 0aad829, pointer 826ac1f→8e62543); ocx [#503](https://github.com/ocx-sh/ocx/pull/503) head `77ae3cc0` verified=true, PR checks 14/14 (Smoke Acceptance hit its 10-min cap once after all tests ran — rerun green), Verify Deep green (14 jobs incl. macOS/Windows unit, Linux acceptance, Satellite Verify) (run 35831653459; earlier head e95d47c2 run 35814067371 also green incl. macOS/Windows); ocx `task verify` exit 0 on the final tree (`.tmp/ocx-upstream-pr/ocx-verify-3.log`); local `satellite:verify` (OCX_MIRROR_DIR=this repo) exit 0; mirror `ocx run -- task verify` exit 0 at 8e62543 (`.tmp/verify-p2-final.log`, 1574 nextest, 202/0/2 acceptance, `REGISTRY=localhost:5021`); Cargo.lock unchanged; oracle 202/0/2 = baseline; e2e tier 2 PASS (dev tag `0.7.0-dev_20260923063955` of 826ac1f — later commits are tests/docs/pointer to a doc/test-only ocx delta); /hex-plan high (2 reviewers, 1 Block fixed) → /hex-execute (L2 review: 1 Warn + 6 Suggest) → /hex-review high Needs Work (2 Warn, 5 Suggest) → fixed; plan `.claude/state/plans/plan_bazel_phase2_ocx_python.md` (local) |
| 3 — Bazel Linux loop (+ CI swap only on pre-declared measured bar) | done | phase3-loop | commits d52ee0e..4b1f40f + ledger (ADR A-8 telemetry, A-9 as-executed; WP-A 04831af Bazel core, WP-T e938bae telemetry, WP-B a32cfc2 unit tests/floors, WP-C 04379c9+aaab22e acceptance, WP-D 44dd60e..034f8b6 wiring/CI/docs, L1 fixes 580d466, review fixes 5136eb7..4b1f40f); `ocx run -- task verify` EXIT=0 at 4b1f40f (`.tmp/p3/verify-2.log`, `REGISTRY=localhost:5021`; also 580d466 tree `.tmp/p3/verify-1.log`); Bazel: 14 unit targets, per-test JUnit 1570 == libtest 1570, 4 `[[excluded]]` rows run by nextest (1570+4 = 1574 = `cargo nextest list`), acceptance target 202/0/2, `bazel:cache:check` second no-change run `cachedLocally: 15`; S-a (incl. `@crates//:ocx_python`) / S-b (submodule clean after fresh repin) / S-c / S-d (`--version`/`--help` byte-equal) PASS; C11 **NO-GO** (warm Bazel 738 s vs nextest 720 s, bar ≤540 s) → `.claude/artifacts/measurement_bazel_ci_lane.md`, `decision_bazel_adoption.md`, CI stays nextest; CI probe (Verify via workflow_call, runs 35844115823 + 35848989870 on 4b1f40f) all green incl. `Bazel graph (Linux)`; telemetry: Tempo 1574 unit + 204 acceptance spans for run 35848989870 under `service.name=ocx-mirror-tests`, `vcs.repository.name=ocx-mirror`, CI BEP summary under `ocx-mirror-bazel-build`; Grafana `repo` var on `ocx-test-time` (v3) + `ocx-bazel-build` (v2), default `ocx`, `=~"ocx-tests"` returns no mirror series; oracle final 202/0/2 cargo + Bazel; e2e tier 2 PASS; /hex-review high Needs Work (0 Block after fixes; quality Block Q1 fixed) → fixes → delta review Pass; plan `.claude/state/plans/plan_bazel_phase3_bazel_loop.md` (local) |
| R — self-refinement (≤3 turns) | done | refine-finalize | 3 turns of `/hex-review high` + `/hex-execute`. T1 (whole branch, 6 seats: spec, quality×3, security, docs): 1 Block (ocx-upstream-pr wip commit rejected by ocx's commit-msg hook), 10 Warn, ~25 Suggest → all actionable fixed incl. the Loop-3 deferrals (`scripts/_gate.py`, `scripts/bazel_scoped.py` + self-test, setup-ocx 0.6.2); L2 on the fix: 2 Warn fixed. T2 (delta + whole-branch adversarial seat): 5 Warn (hook `$PWD` from inside the submodule, rust:verify blind to new test files, acceptance inputs, first-party features/build.rs in drift) → fixed. T3 (delta): 1 Block (`switch -C main && commit` allowed) + 1 Warn → fixed; L1 on that fix: 1 Block (trailing `--`), 1 Warn (`branch -M main`) → fixed. Gates: `ocx run -- task verify` EXIT=0 after each turn (`.tmp/rf/verify-r{1,2,3}.log`) and on the final tree 621310c (`.tmp/rf/verify-final.log`); hook suite 194 passed |
| F — `/hex-finalize`, mirror PR merge-ready | done | refine-finalize | 94 working commits recomposed into 15 on top of the owner's signed 537d591 (each commit = a tree the branch held, docs overlaid; see § Divergences); final tree == pre-finalize tree 3daf87a + this ledger; backup `backup/hex/bazel-crate-split-3daf87a`; force-push with lease pinned to 397fbcd; [#89](https://github.com/ocx-sh/ocx-mirror/pull/89) opened; JUnit gist; pipeline green, § Verification checklist row 9 |

## Meta-plan

Each loop = Opus sub-orchestrator running `/hex-architect` (phase-scoped, amends umbrella ADR only
if proven wrong) → `/hex-plan` → `/hex-execute` → `/hex-review` (loop's work only) → `/hex-execute`
(fixes) → gate (oracle where due, checkpoint commit). Sequential; next starts only from a passed gate.

- Loop 0 — ADR § phase 0: `.claude/rules/crate-placement.md`, skill `.claude/skills/ocx-upstream-pr/`,
  hook `.claude/hooks/pre_tool_use_guard.py` (G1–G5) + pytest + `claude:hooks:test` in verify,
  rewrite `cd external/ocx` lines (update-ocx SKILL.md, README.md), register in CLAUDE.md +
  meta-ai-config.md. Record the v0.6.2 oracle BASELINE (current binary) here.
- Loop 1 — D8: `/update-ocx` bump to ocx origin/main (14 commits), then throwaway Bazel spike S-a…S-d
  in `.agents/worktrees/spike-bazel` (result decides Q1 fallback), then crate split per Q4
  (8 crates, `crates/crate_map.toml` + `tests/workspace_structure.rs`). Gate: oracle.
- Loop 2 — promote `ocx_python` (+ `Pylock::find_package`) into ocx via `ocx-upstream-pr` skill;
  PR green → pointer to pushed verified PR-head SHA; landing = owner action. Gate: oracle.
- Loop 3 — Bazel Linux loop per Q1/Q2/C11, JUnit per-test, `bazel:patch:check`, excluded-tests
  cargo task; CI lane measurement vs C11 bar (expected NO-GO), `Bazel graph (Linux)` CI job.
  Telemetry to otel.ocx.sh like ocx + Grafana repo filter (owner requirement 2026-09-23, § Decisions).
  Gate: oracle vs cargo- and Bazel-built binaries; telemetry visible in Grafana under the repo filter.
- R — ≤3 turns `/hex-review` + `/hex-execute`; F — `/hex-finalize`, push, open mirror PR, green.

## Oracle results

Oracle = worktree of tag `v0.6.2`, its `test/` unmodified, `OCX_MIRROR_COMMAND` at the new
binary; plus current suite and `/e2e-test` tier 2. Runs after phases 1, 2 and at the end.

| When | v0.6.2 suite | current suite | e2e tier 2 | Notes |
|---|---|---|---|---|
| baseline (pre-split) 2026-09-22 | 202 pass / 0 fail / 2 skip (29 s) | 202 pass / 0 fail / 2 skip (inside `task verify`, same skips) | — (not due) | binary = `cargo build --release --locked` of 5b8c1f8 (src/crates/Cargo/test byte-identical to v0.6.2) → `.tmp/oracle-bin/ocx-mirror`; `OCX_COMMAND`=`OCX_TEST_BINARY`= ocx 0.6.2 (`~/.ocx/packages/ocx.sh/sha256/38/325682c914f09d5d69f4f5c7e09db0/content/bin/ocx`); `TMPDIR=~/.cache/ocx-mirror-oracle-tmp`; JUnit `.tmp/oracle-baseline.xml`; `git diff --exit-code v0.6.2 -- test/` clean. Skips (the oracle skip set): `test_mirror_pylock.py:140` (e2e needs real cpython, validated vs dev.ocx.sh), `test_mirror_pypi.py:636` (needs `OCX_TESTS_ONLINE=1`). No pre-existing failures. **Registry divergence from C10.3/5:** :5001 is squatted by the sibling ocx's compose project `test` (`test-mirror-registry-1`, another session's — not torn down), so the run used a dedicated `registry:2` container `ocx-mirror-oracle-registry` on :5021 via `REGISTRY=localhost:5021` (asserted owner before + after); :5002/:5011 came up as compose project `ocx-mirror-test`. **Sigstore divergence from C10.4:** `OCX_SIGSTORE_COMPOSE` left at its default (sibling `../ocx/test/docker-compose.yml`, stack already up) — pointing it at `<wt>/external/ocx/test/…` would re-create the sibling's running `test` project containers. Later oracle runs must use the same env. Worktree `.agents/worktrees/oracle-v0.6.2` **kept** for later loops (remove at end per C10.7, plus `docker rm -f ocx-mirror-oracle-registry`). |
| after phase 1 2026-09-23 | 202 pass / 0 fail / 2 skip (26 s), same skip set | 202 pass / 0 fail / 2 skip (inside `task verify` at e21ba80) | **tier 1 instead** (tier 2 needs the branch pushed + Deploy Dev dispatch — owner-only): 128 specs `validate` exit 0 and 98 repos `generate ci --check` exit 65, stdout/stderr byte-identical between pre-split (9505fde) and split binaries (timestamps masked); hexyl archive + pipx env pipelines plan→prepare→push to :5021 published, runtime proof OK, plan JSON identical to pre-split | binary = `cargo build --release --locked` of e21ba80 → `.tmp/p1-oracle-bin/ocx-mirror` (sha256 867f0be0…); same env as baseline (script `.tmp/p1-oracle.sh`, JUnit `.tmp/oracle-phase1.xml`, log `.tmp/oracle-phase1.log`); :5001 still the sibling's `test` project before + after, :5021 `ocx-mirror-oracle-registry`; `git diff --exit-code v0.6.2 -- test/` clean before + after. e2e logs `.tmp/e2e-p1/` |
| after phase 2 2026-09-23 | 202 pass / 0 fail / 2 skip (25 s), same skip set | 202 pass / 0 fail / 2 skip (inside `task verify` at 8e62543) | **tier 2 PASS**: Deploy Dev run 35827798152 on 826ac1f → `dev.ocx.sh/ocx/mirror:0.7.0-dev_20260923063955`; throwaway `e2e/bazel-crate-split-p2` on ocx-contrib/mirror-actionlint (run 35831877657) + mirror-pypi (run 35831880068, bootstrap 35831883230) all green, cli pinned 0.6.2; platform manifests digest-identical to phase 1 (actionlint 1.7.12, pycowsay 0.0.0.1/0.0.0.2); runtime proof OK; branches deleted; images under `dev.ocx.sh/ocx/e2e/p2/*` left (as phase 1) | binary = `cargo build --release --locked` of 8e62543 → `.tmp/p2-oracle-bin/ocx-mirror` (sha256 3a1847be…); script `.tmp/p2-oracle.sh` (same env as baseline), JUnit `.tmp/oracle-phase2.xml`, log `.tmp/oracle-phase2.log`; :5001 sibling `test` project, :5021 `ocx-mirror-oracle-registry` before + after; `test/` clean vs v0.6.2 before + after. e2e logs `.tmp/e2e-p2/` |
| final (phase 3) 2026-09-23 | **cargo** `cargo build --release --locked` of ffe3563 (Rust tree = 4b1f40f; sha256 3a1847be… = phase-2 binary): 202 pass / 0 fail / 2 skip (28 s); **Bazel** `bazel build -c opt //:ocx-mirror` of ffe3563 (sha256 9034e5fe…): 202 / 0 / 2 (25 s); both: baseline pass set ⊆ pass set, skip set equal | 202 / 0 / 2 inside `task verify` at 4b1f40f (Bazel `//test:acceptance`) and in CI probe 35848989870 | **tier 2 PASS**: Deploy Dev 35844493373 on ffe3563 → `dev.ocx.sh/ocx/mirror:0.7.0-dev_20260923094225`; throwaway `e2e/bazel-crate-split-p3` on mirror-actionlint (35845563705, 15/15) + mirror-pypi (35845567353, 11/11; bootstrap 35845570162), cli resolved 0.6.2 (digest 38325682…); platform manifests identical to phase 2 (actionlint 1.7.12 ×6, pycowsay 0.0.0.1/0.0.0.2 ×4); runtime proof OK; branches deleted (404); images `dev.ocx.sh/ocx/e2e/p3/*` left | scripts `.tmp/p3/oracle-{cargo,bazel}.sh` (baseline env), JUnit `.tmp/oracle-final-{cargo,bazel}.xml`, logs `.tmp/p3/oracle-*.log`; :5001 sibling `test` project, :5021 `ocx-mirror-oracle-registry` before + after; `test/` clean vs v0.6.2 before + after. Oracle worktree + registry container kept for R (remove at F per C10.7). e2e logs `.tmp/e2e-p3/` |

## Decisions

- Owner request 2026-09-23 (post-finalize, lands on #89): add `ocx-mirror version` like ocx
  (`build.rs` provenance + `app::build_info`, `--verbose`, `--format json`) with ocx's `__testing`
  feature (fixed provenance in test builds) so the Bazel/acceptance cache stays stable; prove the
  cache (no-change rerun fully cached; docs-only commit keeps acceptance cached; src change reruns).
  Wire `GIT_SHA_SHORT` in `generate ci` to the build info (dead `option_env!` today).
  → done (version-cmd, 9d874c3, ADR A-11). Choices taken (owner unavailable): Bazel runs no build
  script and reads the `__testing` placeholders from `testing_provenance.env` as `rustc_env_files`
  (one file shared with `build.rs`); `rust:build` + harness build pass `--features __testing` (ocx
  AM-2); rev stamp = 8-char short SHA, `unknown` without `.git`; ocx's release provenance guard
  (`release_provenance_check.py`) not ported — `__testing = []` is a leaf nothing enables and the
  release build passes no feature. `generate ci --check` still compares the header, so binaries of one
  version from different commits now drift-red each other (owner call, § Owner actions).

- Owner requirement added 2026-09-23 (scope of Loop 3): ocx-mirror pushes CI/test telemetry to
  otel.ocx.sh exactly like ocx — port `../ocx/.github/actions/test-telemetry/action.yml`,
  `scripts/bep_to_otlp.py`, `taskfiles/telemetry.taskfile.yml` (+ Bazel BEP→OTLP wiring in
  `.bazelrc`/bazel taskfile, JUnit per-test pushes from verify workflows), with a repo-distinguishing
  resource attribute (e.g. service/`vcs.repository.name` = ocx-mirror; `OTEL_OTLP_AUTH` secret on
  the mirror repo — owner action if absent). Grafana: add a repo filter (dashboard variable) to the
  ocx test/build dashboards so mirror and ocx series separate; verify via the grafana MCP.

- 2026-09-23 Loop 3: telemetry discriminator = own `service.name` per repo (`ocx-mirror-tests`, `ocx-mirror-bazel-build`) +
  `vcs.repository.name=ocx-mirror` (ADR A-8); Grafana `repo` variable defaults to `ocx` (not `All`) so today's ocx view is
  unchanged — `All` is one click; `allValue` unset (a `.*` would match every Tempo service). Dashboards: `ocx-test-time`
  v2→v3 (12 Tempo queries), `ocx-bazel-build` v1→v2 (3 Tempo queries; Prometheus panels unfiltered, description says why).
  Local unit telemetry comes from nextest only (Bazel JUnit has `time="0"`; cached acceptance would re-push old durations).
  C11 NO-GO: the Bazel lane pays the unreplaced cargo steps (ocx submodule build, clippy) cold because C11 gives only the
  nextest lane a cargo cache — replaced surface 379 s → 72 s, net −18 s. Overturning needs a new ADR + fresh runs.
- Owner ruling 2026-09-23: `/ocx-upstream-pr` made model-invocable (ADR A-5); Loop 2 runs it itself, incl. push + PR.
- 2026-09-23 Loop 2: `ocx_python` promoted as [ocx-sh/ocx#503](https://github.com/ocx-sh/ocx/pull/503); ADR A-7 records C5 as
  executed. `find_package` = first entry in lock order, markers not evaluated (PEP 751 allows several entries per name).
  uv re-exports (`ocx_python::uv_pep440`, `uv_distribution_filename`) kept — the uv rev has one authority (ocx). Mirror adapter
  type-pins `Result<Pylock, ocx_python::LockError>` so an upstream error change breaks `satellite:verify` instead of turning
  exit 65 into 1. The PR also fixes ocx main's red `every_public_item_of_ocx_util_has_a_consumer` (`default_threads`, #500)
  by admitting satellite-consumed items that name their consumer. Pointer = pushed PR head (C6.3); ocx `bazel:bootstrap` fails
  on this host (`~/.cargo/config.toml` above the splice) — manual repin with `--repo_env=TMPDIR=/var/tmp/ocx-splice` works.

- Umbrella ADR b2f974ea resolves Q1–Q5: one `@crates` hub from root Cargo.lock (spike-gated, 3
  fallbacks); copy ocx patch handling + `bazel:patch:check`; `ocx_python` ecosystem tier, whole
  crate + `Pylock::find_package`; 8 crates (`ocx_mirror_{test_support,http,report,source,error,spec,
  pipeline}` + root `ocx_mirror` façade); CI bar = ADR § C11 (fixed by b2f974ea, expected NO-GO).
- Verified 2026-09-22: ocx-sh/ocx allows rebase only (squash/merge off), main rulesets
  `required_signatures`, `pull_request`, `non_fast_forward`. Goal forbids rebase-merge → run cannot
  land the ocx PR; pointer goes to pushed verified PR-head SHA (ADR D1).
- 2026-09-23 Loop 1 spike S (ocx f38d22f6, throwaway worktree, removed; output bases expunged): S-a PASS
  (`Build completed successfully, 1318 total actions`), S-c PASS (`"oci-client 0.17.0": {"Path": {"path":
  "<abs>/external/ocx/external/rust-oci-client"}}`, one each for the 3 forks), **S-b FAIL** — a repin
  overwrites ocx's committed `crates/*/BUILD.bazel` (10 files ` M`) and adds `?? BUILD.bazel` in each fork
  (rules_rust `render_build_files`, no switch). Verdict: Q1 O1 kept; `bazel:bootstrap` step (1) = restore
  after every repin + fork `info/exclude` + clean post-condition (ADR A-4, 8ff5cab). Upstream rules_rust
  fix = owner action. Exec form `ocx package exec ocx.sh/bazelbuild/bazel:9.2.0 -- bazel`; host
  `~/.bazelrc` already covers libstdc++ and overrides `--jobs` (pass it explicitly).
- 2026-09-23 Loop 1 step A: pointer 191b9324 → f38d22f6 (61eafc4, README procedure per meta ruling);
  adoption 9916fba imports `PUSH_CHUNK_SIZE`/`REGISTRY_{READ,CONNECT}_TIMEOUT`/`default_threads` from ocx.
  Manifests + Cargo.lock unchanged by the bump. Pre-existing feature-list drift vs ocx, NOT changed
  (output-affecting, not Hat-1): `quick-junit` 0.6 vs ocx 0.7 (default-features=false), `serde_json`
  `preserve_order`, dev `tokio` `test-util` — for the owner's `/update-ocx` review.
- 2026-09-23 Loop 1 crate split: `ocx_mirror_{test_support,http,report,source,error,spec,pipeline}` + root
  `ocx_mirror` + `ocx_python` member; map `crates/crate_map.toml`, enforced by `tests/workspace_structure.rs`
  (clauses a–h incl. ocx's satellite regex and `version.workspace`), `tests/source_scan.rs`, `tests/log_targets.rs`
  (recipe read from `docs/reference/cli.md`). Generic crates return leaf local errors (Display byte-equal,
  identity table in `ocx_mirror_error`). Root keeps private module aliases for `command/` (plan D-P1).
  `jsonschema:check` now `--workspace`. `release.yml` reads the version via `cargo metadata` (workspace-inherited
  version). Cross-crate `#[cfg(test)]` shortcuts inventoried (plan D-P6): retry ladder tests moved into the
  pipeline crate; root keeps two ~1 s end-to-end retry tests.
| version-cmd 2026-09-23 | **cargo** `cargo build --release --locked` of 9d874c3 (sha256 70882adf…, real provenance `commit: 9d874c3d (clean)`): 202 / 0 / 2 (27 s); **Bazel** `//:ocx-mirror` of 9d874c3 (sha256 d668f314…, `channel: test`): 202 / 0 / 2 (28 s); skip set = baseline | `task verify` EXIT=0 at 6dc6300 (`//test:acceptance` PASSED incl. `test_version.py`) | — (not due) | fresh worktree of v0.6.2 needed `submodule update --init external/ocx` (signing fixtures read `external/ocx/test/…`); baseline env (`REGISTRY=localhost:5021` on a fresh `ocx-mirror-oracle-registry`, :5001 = sibling `test` project; `TMPDIR=~/.cache/ocx-mirror-oracle-tmp`); script `.tmp/vc/oracle/run.sh`, logs/JUnit `.tmp/vc/oracle/`; worktree test/ clean after; worktree + container removed |

## Divergences from the artifact (with reasons)

- R/F 2026-09-23: `/hex-execute` ran its WPs file-disjoint in the shared tree, not per-WP worktrees (a worktree needs the nested-submodule checkout; builders barred from state-changing git). `/hex-finalize` recomposed by `commit-tree` over trees the branch held (phase-end states, ADR/discussion folded into commit 1, hook/skill final content into commit 2, refinement fixes into the Bazel/test/doc commits) instead of `reset --soft` + restaging, and kept the owner's signed 537d591 (local main, not on origin) unchanged as the base — restaging would have dropped its signature. Series commits are unsigned (no ssh-agent; mirror `main` ruleset has no `required_signatures`). The turn-2/3 cross-model seat was an opus adversarial reviewer (codex forwarder unreliable per memory).

- Meta ruling 2026-09-23: Loop 1 step A (D8 bump) runs the README `Bumping ocx` procedure, not `/update-ocx` — the skill is `disable-model-invocation` and its refusal must be respected; semantic adoption review → owner action.

- Loop 1: ADR A-4 (S-b fail → bootstrap restore step replaces the `info/exclude`-only step); ADR A-6 (C1 re-exports only for paths that were public — pipeline grammar re-exports dropped). `/e2e-test` tier 2 not run (needs a push) — tier 1 differential instead.
- ADR D1–D8 (see ADR § Divergences). Material: D1 (no run-side ocx merge — squash disabled
  upstream), D6 (acceptance = one cached target running `pytest -n auto`), D8 (ocx bump + spike
  open phase 1).

## ADR amendments

- 2026-09-22 Loop 0 — ADR § Amendments A-1..A-3 (commit dda42d2): A-1 C6.1/C9(b) commit is built on a
  `<branch>-stage` ref then the PR branch is force-moved (resetting an open PR head to base closes the
  PR); `--raw -z --no-renames` parser; explicit https fetch. A-2 C6.2 squash also needs the ruleset's
  `allowed_merge_methods` ∋ squash + 1 approval (today: rebase only) → C6.3 is the expected path.
  A-3 C9 G3 exempts lexical `…/external/ocx` targets; G5 read list widened; update-ocx rewrites cover
  every git line that relied on the removed `cd`.
- 2026-09-23 Loop 1 — ADR § Amendments A-4 (8ff5cab: spike S-b fails, bootstrap restores the submodule after
  every repin; exec form `ocx package exec`; S-c frozen suffix form) and A-6 (29b3447: C1 re-exports only where
  the old path was public; pinned homes of moved items; spec's private `use crate as spec;` keeps schema doc bytes).
- 2026-09-23 Loop 3 — A-8 (d52ee0e: C12 telemetry — own service names, `vcs.repository.name`, Grafana `repo` default `ocx`) and
  A-9 (ffe3563, refined 4b1f40f: exec form is `ocx exec bazel` once `ocx.toml` has the row; bootstrap restores by generated
  header + repins a stale lock; acceptance runner uses the binary behind the `@tools` launcher; widened stamp; extra acceptance
  inputs; wiring incl. `scripts:self-test`, `run: once` bootstrap, OTEL scrubbed from Bazel client calls).
- 2026-09-23 version-cmd — A-11 (root `build.rs` provenance + `__testing`; Bazel reads the placeholders as
  `rustc_env_files`; drift admits that one script; cache proof). A-10 was already taken by refine-finalize.

## Deferred (GitHub issues)

- R (owner call, not filed): end-to-end `max_retries: 2` push-ladder wiring check at the root (D-P6 speed trade-off, +2 s); `bazel:pin:check`/`patch:check` inline heredocs without a scripted self-test (both fail closed); `rust:verify` does not run the 4 nextest-only `[[excluded]]` tests (`task verify` does; stated in its desc); `.bazelrc` `build:ci` unused since NO-GO (ADR C7 requires it); `release:prepare` on macOS/Windows unproven (needs `ocx exec bazel`) — Linux-only or not is the owner's call; ADR still `Status: Proposed` — acceptance is the owner's; hex federation to `../ocx` (`mirror-signing` plan) is blocked by guard G5 — retire the federation or exempt it; subsystem-mirror.md module-map rows missing for modules predating the branch (`sign_backfill`, `catalog_config`, `strip_components_config`).
- Loop 3 deferrals resolved in R: gate helpers → `scripts/_gate.py`; `test:scoped` → `scripts/bazel_scoped.py` with `--self-test`; `oci-publish.yml` setup-ocx → 0.6.2. `workflow-feature.md:65` upstream re-sync note stays for the next port.

- Loop 3 (owner call): `Finding`/`codes`/`report`/`expect` are copied into 3–4 gate scripts (kept diffable against ocx's copies
  instead of one `scripts/_gate.py`). `test:scoped` is an inline Python heredoc without a self-test. Ported
  `.claude/rules/workflow-feature.md:65` lists the non-Linux `task verify` set — flag for the upstream re-sync.
  `oci-publish.yml` pins setup-ocx 0.6.0 while its comment says it tracks the submodule (0.6.2).

- Loop 1 (not filed — owner call): `ocx_mirror_pipeline` exposes all 19 modules `pub` (~100 items with no outside
  user, invisible to `dead_code`); apply the spec crate's private-modules + curated `pub use` pattern in a follow-up.
- Loop 1: pre-existing copy-exactly drift vs ocx not changed (output-affecting, not Hat 1): `quick-junit` 0.6 vs ocx
  0.7 `default-features=false` — for the owner's `/update-ocx` review.
- Loop 1: rules_rust `render_build_files` writes BUILD.bazel into non-member path crates (no upstream issue found) — filing upstream is outward-facing, owner call.
- Loop 2 (owner call): does ocx treat `ocx_python`'s outputs (repack determinism, `REPACK_VERSION`, wheel-reference naming) as an
  interface-tier persisted format with its own subsystem rule, or only as ecosystem API? Nothing in ocx records it yet.

- Loop 0 (not filed yet — owner call): G5 keeps `branch/remote/tag/config/stash` blocked in the sibling
  (write forms need argument inspection); hook ceiling — unquoted `-C $(…)/external/ocx` and a quoted
  `<<X` argument misread as a heredoc; ocx-upstream-pr refuses modifying a `100755` file (whether
  `createCommitOnBranch` preserves the mode on modify is unverified).

## Owner actions (collected; surfaced at the end)

- Decide whether `generate ci --check` should ignore the `(rev …)` header stamp. Today it compares it, so two
  builds of one version from different commits (dev builds, a local build vs the release) red each other with 65;
  the version already made the check binary-specific. Recommended: keep (the header states which binary rendered).
- Decide whether to port ocx's release provenance guard (`release_provenance_check.py`, byte scan for the
  `__testing` markers before publish). Not ported: the feature is a leaf and no release path enables it.
- File upstream (outward, owner call): ocx's `crates/ocx_cli/build.rs` bakes `VERGEN_IDEMPOTENT_OUTPUT` as the
  commit SHA on a checkout without `.git` — vergen reports it in `add_instructions`, not `build()`, so its
  graceful-degrade arm is dead. Mirror fix: own `fail_on_error` emitter (ADR A-11 (5)).
- Delete the 5 GB no-`.git` probe copy the sandbox refused to `rm -rf` (tmpfs, i.e. RAM):
  `rm -rf /tmp/claude-1000/-home-mherwig-dev-ocx-mirror/a8883046-16f8-43db-b19c-2f8d98c34f96/scratchpad/{nogit,gen}`.

- Port the Grafana `repo` variable into `herwig-systems/server-hetzner1` `monitoring/grafana/dashboards/{test-time,bazel-build}.json`
  (file-provisioned, `allowUiUpdates: true` — a future file `version` bump would drop the UI edit): export with
  `curl -s -u … https://<grafana>/api/dashboards/uid/ocx-test-time | jq .dashboard` (and `ocx-bazel-build`), commit, `task git:push`.
- Delete scratch the agent sandbox could not remove: `rm -rf /var/tmp/ocx-mirror-sd ~/.cache/ocx/bazel-root/59ad284773a2ad66bb28d6caac9bd95d`.

- ~~Land ocx-sh/ocx#503~~ — done by owner 2026-09-23 (`15946973`); pointer re-bumped. `feat/ocx-python-crate` may now be deleted.
- No mirror release until an ocx tag contains the pointer SHA (`release:prepare` check enforces).
- ~~`sudo dnf install libstdc++-devel`~~ — not needed on this host: spike S found `~/.bazelrc` already sets `--linkopt/--host_linkopt=-L~/.cache/ocx/libdir` (it also sets `--jobs=12`, overriding the workspace rc; pass `--jobs` explicitly).
- Push `hex/bazel-crate-split` + `gh workflow run "Deploy Dev" --ref hex/bazel-crate-split` to get `/e2e-test` tier 2 for the split (run reached tier 1 only).
- File the rules_rust issue: `crate_universe` writes `BUILD.bazel` into non-member path crates' source folders (ADR A-4).

- Run `/update-ocx` on `hex/bazel-crate-split` for the semantic adoption review of the Loop-1 ocx bump (Loop 1 bumps via README procedure; skill is user-only).

## Spawn log

| # | When | Role | Model | Task | Result |
|---|---|---|---|---|---|
| 1 | 2026-09-22 | sub-orchestrator umbrella-architect | opus | `/hex-architect high` on artifact, resolve Open questions, write umbrella ADR | done b2f974ea |
| 2 | 2026-09-22 | sub-orchestrator phase0-loop | opus | Loop 0 (AI config + oracle baseline) | done cb190dd (+ ledger commit) |
| 3 | 2026-09-23 | sub-orchestrator phase1-loop | opus | Loop 1 (ocx bump D8, Bazel spike S-a…S-d, crate split, oracle) | done 7bfd2b8 (+ ledger commit) |
| 5 | 2026-09-23 | sub-orchestrator phase2-loop | opus | Loop 2 (`ocx_python` promotion, ocx PR, pointer, oracle, e2e tier 2) | done 8e62543 (+ ledger commit); ocx-sh/ocx#503 open, green |
| 6 | 2026-09-23 | sub-orchestrator phase3-loop | opus | Loop 3 (Bazel Linux loop, JUnit, CI bar C11, OTEL telemetry + Grafana repo filter, final oracle) | done 4b1f40f (+ ledger commit); C11 NO-GO; telemetry live |
| 7 | 2026-09-23 | sub-orchestrator refine-finalize | opus | R (≤3 turns /hex-review + /hex-execute on whole branch) + F (/hex-finalize, mirror PR, green pipeline, Verification items) | done — [#89](https://github.com/ocx-sh/ocx-mirror/pull/89) |
| 8 | 2026-09-23 | sub-orchestrator version-cmd | opus | `version` command + `__testing` provenance + cache proof on #89 | done 9d874c3 + f58c79b (CI fix: release nextest relinked the uploaded binary without `__testing`); ADR A-11; cache proof 4/4; `task verify` EXIT=0 at f58c79b; Verify [35890579251](https://github.com/ocx-sh/ocx-mirror/actions/runs/35890579251) green |

## Verification checklist (artifact § Verification + ADR § Phase plan and gates)

| # | Item | Status | Evidence |
|---|---|---|---|
| 1 | `task verify` green on the final branch | met | `ocx run -- task verify` EXIT=0 on tree 621310c (`.tmp/rf/verify-final.log`, `REGISTRY=localhost:5021`); CI on [#89](https://github.com/ocx-sh/ocx-mirror/pull/89) |
| 2 | v0.6.2 oracle green after phases 1, 2, end; `test/` unmodified | met | § Oracle results rows; plus refinement re-run on 621310c's Bazel `-c opt` binary (sha256 9df54152…): 202/0/2, same skips, `test/` clean vs v0.6.2 before + after (`.tmp/rf/oracle-bazel.log`) |
| 3 | Current suite green, also as cached `bazel test`; second no-change run fully cached | met | `//test:acceptance` PASSED in `task verify` (verify-r3), `(cached) PASSED` in verify-final on the identical tree; `bazel:cache:check` in both runs |
| 4 | `/e2e-test` tier 2 green | met | phase 3: Deploy Dev 35844493373 → mirror-actionlint 35845563705 (15/15), mirror-pypi 35845567353 (11/11); R changed no production Rust (test files + a lint attribute only) |
| 5 | Per-test JUnit published to the PR, count == libtest total | met | 1570 cases / 14 targets == libtest 1570 (verify-final floor line); [gist](https://gist.github.com/michael-herwig/cfd7b9d994ba86f39b1d20bda4a32b02) linked from the PR body (C11 NO-GO clause) |
| 6 | ocx PR squash-merged; pointer = `merge_commit_sha`; `cargo tree -i oci-client` fork | met (landed by owner) | [ocx-sh/ocx#503](https://github.com/ocx-sh/ocx/pull/503) landed as `15946973` on ocx main (tree identical to verified head `77ae3cc0`); `external/ocx` → `15946973`; `cargo tree -i oci-client` → `external/ocx/external/rust-oci-client`; `cargo tree -i ocx_python` → `external/ocx/crates/ocx_python` |
| 7 | ocx `satellite:verify` green with the new crate | met | Verify Deep run 35831653459 (Satellite Verify) on #503 head + local run (phase-2 row) |
| 8 | CI lane-swap decision recorded with numbers | met | `measurement_bazel_ci_lane.md` (NO-GO, run 35846656688), `decision_bazel_adoption.md` |
| 9 | Mirror PR green, finalized via `/hex-finalize`, left for the owner | met | [#89](https://github.com/ocx-sh/ocx-mirror/pull/89): Verify run [35867476966](https://github.com/ocx-sh/ocx-mirror/actions/runs/35867476966) on b581093 all 6 checks green (Conventional Commits, Smoke, Bazel graph, Acceptance, both result publishers), first attempt, no reruns; merge state BLOCKED only on the required approval; not draft, no auto-merge. The ledger-only amend that records this re-ran Verify on the final head (see PR) |
| G0 | `claude:hooks:test` green | met | 194 passed (verify-final) |
| G1 | workspace_structure, count gate, log-filter, schema byte-compare | met | phase-1 gates (`.tmp/fix2-gates.log`); structural tests in every verify |
| G2 | `cargo tree -i ocx_python` resolves the submodule crate | met | row 6 |
| G3 | all `bazel:*` gates, JUnit == libtest, coverage gate, oracle cargo + Bazel | met | verify-final; § Oracle results final row + row 2 |

## Next action

Owner review of [#89](https://github.com/ocx-sh/ocx-mirror/pull/89) (owner actions in the PR body and § Owner actions). version-cmd cleanup done: oracle worktree, registry container, Bazel server; the scratchpad probe copy is an owner action (sandbox refused `rm -rf`).
