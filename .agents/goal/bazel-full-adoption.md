# Goal ledger — full Bazel adoption (ocx parity)

State: awaiting owner merge · Branch: `hex/bazel-full-adoption` (off origin/main 787241c) · Started: 2026-09-23

## Goal (owner, verbatim)

> Copy the ocx bazel setup, but look a the current state of @../ocx it is actively devloped, also double
> check its .agents plans/adr/discussion about the bazel integration. Hard Requirement is that all parts are
> done with bazel, incl. compilation, unit/acceptance test execution, website build and whatsoever. Look to
> exploit high cacheability using our cache server locally and in ci, two consectuive runs in the ci should be
> fully cached (notably we have the __testing feature for that with the placeholder data). If in doubt check
> how @../ocx does it, because I discussed there a lot already.

## Why (the miss this fixes)

The previous goal (`.agents/goal/bazel-crate-split.md`) put Bazel in the Linux dev loop only, measured a CI
bar with **no remote cache** (C11 → NO-GO, `.claude/artifacts/decision_bazel_adoption.md`), and left CI on
cargo/nextest. ocx meanwhile runs `bazel test //crates/...` in Smoke against `bazel-cache.ocx.sh`.

## Sources of truth in ocx (READ-ONLY: `/home/mherwig/dev/ocx`, never write there)

- Local checkout is on `hex/test-speed-tiers` (46 commits ahead of origin/main, in flight) — read both it and
  `origin/main` (`git -C /home/mherwig/dev/ocx show origin/main:<path>`).
- `.claude/artifacts/`: `adr_bazel_build_adoption.md`, `plan_bazel_build_adoption.md`,
  `discover_bazel_full_adoption.md`, `migration_plan_bazel.md`, `measurement_bazel_r2.md`,
  `research_bazel_cache_trust_boundary.md`, `research_bazel_*`, `review_adr_bazel_*`,
  `adr_test_speed_tiers.md`, `plan_test_speed_tiers.md`, `system_design_test_tiers.md`.
- `.agents/discussions/test-suite-speed-tiers.md`.
- `.bazelrc`, `MODULE.bazel`, `BUILD.bazel`, `website/{BUILD.bazel,site.bzl}`, `test/BUILD.bazel`,
  `test/doc_scripts/`, `taskfiles/bazel.taskfile.yml`, `.github/actions/bazel-cache-rc/`,
  `.github/workflows/verify-{basic,deep}.yml`.

## Facts established by the meta loop

- Org secrets `BAZEL_CACHE_READ_AUTH` / `BAZEL_CACHE_WRITE_AUTH` have visibility `selected` = {ocx, rules_ocx};
  ocx-mirror was not in the list — **added by meta loop 2026-09-23** (now {ocx, ocx-mirror, rules_ocx}).
- Developer host `~/.bazelrc` already carries a read `--remote_header` for `bazel-cache.ocx.sh`.
- ocx `.bazelrc`: `--remote_cache=https://bazel-cache.ocx.sh/v1` (generation = URI path, `--remote_instance_name`
  is inert over HTTP), `--remote_upload_local_results=false` (writes granted per lane, main push only),
  `build:ci --remote_download_minimal` + `--remote_download_regex=.*/test\.(xml|log)$`, `build:ci` noted LATENT
  in ocx.
- Mirror CI today: `verify.yml` Smoke = nextest + cargo; `bazel-graph` job = static gates + `--nobuild` only;
  `docs.yml` = mkdocs outside Bazel; `build-matrix.yml` / `release.yml` = cargo.

## Done bar (the Stop-hook condition, made checkable)

1. Every CI lane ocx runs through Bazel runs through Bazel here, plus the mirror-only parts: compile, clippy/fmt
   where ocx does, unit tests, acceptance tests, mkdocs site build. Anything left on cargo is left there **only
   because ocx leaves the same thing on cargo** (cite the ocx file) — recorded below.
2. Remote cache wired locally (rc, read) and in CI (bazel-cache-rc port; write on main push per ocx's trust model).
3. **Two consecutive CI runs fully cached**: evidence = two run URLs on the same tree (or a docs-only /
   no-op commit), second run shows 0 executed actions / all tests `(cached)` for every Bazel lane.
4. Local: a second `bazel test //...` after `bazel clean` is served from the remote/disk cache (numbers here).
5. New ADR superseding C11 of `adr_bazel_crate_split.md`; `decision_bazel_adoption.md` updated.
6. `task verify` green; mirror PR open for the owner (never merged by agents, never pushed to main).

## Rules carried over

- ocx changes only in `external/ocx` (branch first, `git -C <abs>`); guard hook G1–G5 applies.
- `target/` and `/tmp` wiped hourly → long logs in project `.tmp/` (gitignored), deleted at the end.
- RAM: WSL aborts on OOM — one Bazel server, `--jobs` capped, `bazel shutdown` when idle.
- No SSH agent: commits unsigned or GraphQL `createCommitOnBranch`.
- Commit hook may be skipped only in the loop, never at finalization.

## Log

- 2026-09-24 meta: #92 green on all six release legs (run 35987621873), #91 Verify green with the credential fix (run 35987584631); #92 folded into #91, series regrouped by concern (final tree = 93b6d7a's + this ledger), #92 closed.
- 2026-09-24 sub-orch: `hex/bazel-release-builds` head 93b6d7a contains #91 tip 3522ebe (ff-able); all 6 legs on Bazel, green in [35987621873](https://github.com/ocx-sh/ocx-mirror/actions/runs/35987621873); no cargo leg left. bazel-release worktree removed (branch kept, pushed).
- 2026-09-24 sub-orch: #91 Verify [35987584631](https://github.com/ocx-sh/ocx-mirror/actions/runs/35987584631) green with the credential fix: 0×401, 171 remote cache hits (third-party, from ocx's /v1 writes). #92 Build Check [35987621873](https://github.com/ocx-sh/ocx-mirror/actions/runs/35987621873) green on all 6 legs; #92 marked ready. Returning.
- 2026-09-24 sub-orch: phase M+W done on #92: all 6 release legs via Bazel (macOS native+apple_support, Windows native MSVC + 2 rules_rust patches for Windows path/PATH-length bugs), parity tables in `.tmp/bazel-full/release-parity.md`, build-check [35984021713](https://github.com/ocx-sh/ocx-mirror/actions/runs/35984021713) green. Phase-W worker found a #91 defect: docs.taskfile vars shadowed bazel.taskfile's CACHE_RC (go-task 3.53) → every CI Bazel lane ran anonymous (401 warnings), so a main push would have uploaded nothing. Fixed on #91 (698647e) + guard `bazel:cache:rc:check` in verify.yml (3522ebe, proven red on the old vars); #92 rebased, guard added to its release legs.
- 2026-09-24 meta: owner won't merge #91 until finalized; decided: fold #92 into #91, finalize as grouped commits (cache-rc action / Bazel lanes / docs site / release builds / workflows / ADR+docs / ledger), same final tree. Meta loop does the fold + finalize after bazel-full reports #92 green.
- 2026-09-24 sub-orch: release phase L done: Linux musl x2 via Bazel (hermetic_cc_toolchain 4.3.0/zig), real provenance via `--define=ocx_mirror_provenance=release` + workspace status (no-remote-cache action), parity vs cargo-zigbuild recorded (`.tmp/bazel-full/release-parity.md`), host keys unchanged (1143/1144, the 1 = an edited declared input). Draft PR [#92](https://github.com/ocx-sh/ocx-mirror/pull/92) build-check green ([35942522657](https://github.com/ocx-sh/ocx-mirror/actions/runs/35942522657)). Next: macOS, then Windows.
- 2026-09-24 sub-orch: meta round 3: release + cross-platform builds to Bazel on stacked branch `hex/bazel-release-builds` (worktree `.agents/worktrees/bazel-release`). #91 frozen except CI-red fixes.

- 2026-09-23 meta: branch created, ledger written, sub-orchestrator spawned.
- 2026-09-24 sub-orch: meta round 2 closed: verify.yml runs no cargo and has no Rust setup; drift gate's wrapper build now carries the upload grant (2554774). CI [35936219566](https://github.com/ocx-sh/ocx-mirror/actions/runs/35936219566) green; keys identical to [35934619032](https://github.com/ocx-sh/ocx-mirror/actions/runs/35934619032) except the 2 self-tests whose inputs changed. `task verify` exit 0 on 2554774. Returning.
- 2026-09-24 sub-orch: meta round 2: WP-D (clippy/rustfmt aspects, jsonschema variants, fork check as lockfile read, no cargo/Rust setup in verify.yml) + WP-E (9 py_tests for gate scripts/telemetry) landed; `task verify` exit 0 on d4f3085; CI run 35934619032 running.
- 2026-09-24 sub-orch: final `task verify` exit 0 on 5d3379e (`.tmp/bazel-full/verify2.log`); CI [35930299279](https://github.com/ocx-sh/ocx-mirror/actions/runs/35930299279) green, keys identical to runs 1–3. PR [#91](https://github.com/ocx-sh/ocx-mirror/pull/91) ready for review. Done-bar 1,2,4,5,6 met; 3 = pre-merge half met, post-merge half is the owner action below. Sub-orch returning.
- 2026-09-24 sub-orch: done-bar 3 pre-merge half: run [35925312539](https://github.com/ocx-sh/ocx-mirror/actions/runs/35925312539) attempts 2 vs 3 (same tree) — execlog action keys byte-identical (unit 1125, accept 1, site 2; generate-xml spawns excluded, fixed in 5d3379e); run 1 vs run 2 identical too. Post-merge half (actual hits) needs a main push → owner action. Final `task verify` running on 5d3379e.
- 2026-09-23 sub-orch: CI run 1 [35923034539](https://github.com/ocx-sh/ocx-mirror/actions/runs/35923034539) green (Smoke 17 min, all cold: unit `1590 processes: 1139 processwrapper-sandbox`, 0 remote hits — nothing written yet). Opus review: 0 Block, 3 Warn fixed in 6a462b2 (templates declared, _MEMBER_SOURCES drift check, tag run gets read secret). Pushed; run 2 = first run on the final tree, run 3 = rerun of it for execlog key diff.
- 2026-09-23 sub-orch: done-bar 4 (local): after `bazel clean`, `bazel test //test:acceptance //docs:site` → `1523 processes: 1099 disk cache hit, 424 internal`, `Executed 0 out of 1 test`; `//...` after clean → 14/15 `(cached)`, 1141 disk hits (acceptance re-ran once: first run after a `task verify` under `ocx run`, env differed; bare→bare is 0 executed). Logs `.tmp/bazel-full/local-item4.log`, `l4.log`.
- 2026-09-23 sub-orch: local `task verify` exit 0 on 813e6ca; branch pushed; draft PR [#91](https://github.com/ocx-sh/ocx-mirror/pull/91) open, CI run 1 started.
- 2026-09-23 sub-orch: WP-A/B/C done and cherry-picked onto the goal branch (MODULE.bazel.lock regenerated); ADR `adr_bazel_full_adoption.md` written (done-bar 5); local `task verify` running.
- 2026-09-23 sub-orch: explorers back; decisions D1–D5 recorded; spawning WP-A/B/C.
- 2026-09-23 sub-orch: started; fanning out 3 sonnet explorers (ocx rulings, ocx config inventory, mirror CI/harness inventory).

## Design decisions (sub-orch, 2026-09-23)

Inputs: `.tmp/bazel-full/{ocx-rulings,ocx-inventory,mirror-inventory}.md` (explorer reports).

- D1 cache: port ocx `.bazelrc` remote block verbatim, **share `/v1`** with ocx (same host, same secrets, same
  realm; ocx gives no reason against; cross-repo hits on third-party crates are a bonus). `bazel-cache-rc` action verbatim.
- D2 write lane: one job (verify.yml `smoke`), `push` to `main` only, gate additionally `github.workflow == 'Verify'`
  because verify.yml is also a `workflow_call` target (ocx ruling 5a). Upload grant as CLI arg, never in rc.
- D3 everything that runs in CI under Bazel is remote-cacheable (unit, acceptance, site) — owner bar; see deviations.
- D4 WPs: A = cache + CI lane (main checkout); B = acceptance hermeticity + `[[excluded]]` → Bazel + tag guard
  (worktree `accept`); C = mkdocs site under Bazel, hermetic Python deps, docs.yml deploys Bazel output (worktree
  `site`). Bazel/cargo runs serialized host-wide via `flock .tmp/bazel-full/bazel.lock` + `bazel shutdown`.
- D5 evidence pre-merge: CI writes only on main push, so a branch cannot show cache *hits*; pre-merge evidence = two
  branch runs with identical action keys (execution-log diff) + local item 4. Post-merge: main push run + re-run.

## Deviations from ocx (each with reason)

Full table: `.claude/artifacts/adr_bazel_full_adoption.md` § Deviations. Summary:
- Acceptance results remote-uploaded (ocx: host-only) — owner's fully-cached bar; inputs fully declared (`@ocx_test`, stamp removed, env_inherit trimmed, tag guard).
- Docs site remote-cacheable + deployed from Bazel (ocx: `no-remote-cache`, deploy bypasses Bazel) — rules_python hashed lock makes the action offline.
- `--config=ci` wired (ocx: latent); acceptance in the Smoke job (ocx: verify-deep); `github.workflow == 'Verify'` write-gate term (verify.yml is also workflow_call); execution-log key artifact (pre-merge evidence).
- Kept on cargo, each as ocx keeps it: fmt, clippy (ocx verify-basic.yml "Check formatting"/"Clippy"), jsonschema check (ocx `schema:generate`), release + cross-platform (ocx release.yml has no Bazel; darwin/windows legs nextest). `cargo tree -i` fork assertion kept: mirror-only `[patch.crates-io]`, release is cargo.


## Owner actions

- Merge [#91](https://github.com/ocx-sh/ocx-mirror/pull/91) (#92 folded in and closed). Only a `main` push writes the shared cache.
- Then re-run that first `main` Verify run (`gh run rerun <id>`, or tell the meta loop): every test step must show `Executed 0 out of` — the last open done-bar item (3).
- Decide whether to upstream the two rules_rust Windows patches (`release/rules_rust_windows_*.patch`, bazelbuild/rules_rust#3767) so the override can go.
- Optional: host `~/.bazelrc` `build --linkopt=-L…/libdir` re-keys every local link action, so this host never reuses CI's linked test binaries. Drop it if nothing needs it.
