# Rebase Dossier — `feat/pypi-mirror` onto `origin/main`

**Status**: Analysis complete. Sole spec for the rebase executor.
**Date**: 2026-08-04
**Merge base**: `75328b6` · **Branch tip**: `ed26b9d` · **Target**: `origin/main` = `d200b72` (v0.5.2)
**Main commits to absorb**: 50 (not 94 — `git log 75328b6..origin/main` is 50, no merges)
**Directive**: main defines the new design direction. The branch adapts to main, never the reverse.

---

## 0. Executive decisions (read first — these reshape the whole rebase)

### D-1 — DROP the workspace conversion. Keep `ocx_mirror` at the repo root.

`5da32e2` moved `src/**` → `crates/ocx_mirror/src/**` and `tests/**` → `crates/ocx_mirror/tests/**`.
**Main never adopted this.** 50 commits later `origin/main` is still single-crate at the repo root:

```toml
# git show origin/main:Cargo.toml  (lines 1-21)
[package]
name = "ocx_mirror"
version = "0.5.2"
edition = "2024"
...
[workspace]
exclude = ["external/ocx"]

[[bin]]
name = "ocx-mirror"
path = "src/main.rs"
```

Per the directive, main's layout wins. The conversion is undocumented (`5da32e2`'s subject says
"pylock source type, env pipeline & container CI" — the workspace conversion is unmentioned), and
it buys nothing that cargo does not already give for free: **a path dependency residing inside the
workspace directory automatically becomes a workspace member.** So `crates/ocx_python` needs
exactly one line to exist as a sibling crate:

```toml
# root Cargo.toml, [dependencies]
ocx_python = { path = "crates/ocx_python" }
```

No `[workspace] members`, no `[workspace.package]`, no `[workspace.dependencies]`, no virtual
manifest. `[patch.crates-io]` stays exactly where it already is (root `Cargo.toml:80-82`).

**What dropping the conversion buys** — every one of these becomes a non-issue instead of a task:

| Cost the conversion imposes | Evidence |
|---|---|
| Every hunk in main's 50 commits under `src/**`/`tests/**` needs a path remap | 48 files are pure R100 renames in `5da32e2` |
| Main's `tests/golden/*.txt` (8 new files) must relocate, or every golden test fails | `ci.rs:3676` `concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden")` — resolves to the *member* dir |
| Main's new `src/command/package/pipeline/patch.rs` + `announce.rs` + `spec/{bin_scan,announce_config}.rs` get orphaned at `src/` while the rest of the tree moved | main added them after the branch's rename commit was written |
| `renovate.json` customManagers break — both are `^`-anchored to `src/...` | `test/tests/test_renovate_managers.py` fails loudly: "customManager matches nothing" |
| `taskfiles/rust.taskfile.yml:7-10` `sources:` globs stop matching → Task change-detection silently no-ops | globs are `{{.ROOT_DIR}}/src/**/*.rs` |
| `.claude/rules/subsystem-mirror.md` frontmatter `paths: [src/**, tests/**]` stops auto-loading the rule | frontmatter identical on both sides |
| `taskfiles/release.taskfile.yml` `cargo set-version` (no `-p`) becomes ambiguous; `release.yml:42` `grep -m1 '^version' Cargo.toml` breaks | `f4264f4` |
| `.github/actions/build-rust/action.yml:46,52` + `build-matrix.yml:100` `cargo build` (no `-p`) start cross-compiling `ocx_python` for every target incl. windows-msvc/zigbuild | worker-verified |
| `UPDATE_GOLDEN=1 cargo test -p ocx-mirror` package selector needs revisiting | `ci.rs:3673` |
| `.licenserc.toml` globs | branch already changed these — revert |

**Executor action**: replay `5da32e2` **without** the relocation. Keep `src/`, `tests/fixtures/`,
`tests/golden/` at the repo root. Revert the branch's edits to `.licenserc.toml`,
`taskfiles/rust.taskfile.yml`, `taskfiles/release.taskfile.yml`, `.github/workflows/release.yml`
back to main's versions — all four exist only to serve the conversion.

`crates/ocx_python/` stays exactly where it is. It is a genuinely new crate and main has no opinion
on it.

### D-2 — BLOCKER: the branch's `external/ocx` pointer is a **fork**, not an ancestor.

```
c04f3697  (merge-base)
   ├── ed358593  ← branch pointer: c04f3697 + 6 commits that exist ONLY here
   └── e4c640dd  ← origin/main pointer, ocx v0.5.3
```

`git -C external/ocx merge-base --is-ancestor ed358593 e4c640dd` → **NO**. Six ocx commits are
branch-only:

| sha | subject |
|---|---|
| `f2e4e54e` | feat(oci): surface cross-repo blob mount through transport and publisher |
| `b49515d7` | feat(cli): `from=` layer-ref source and layer push counters |
| `b91f7898` | feat(oci): consume typed `BlobMountResponse` from the bumped fork |
| `63c79e3b` | fix(cli,lib): adapt v0.4.2 call sites to layer-mount surface |
| `ddca3ba2` | fix(shim): allow OCX_HOME/temp package roots (package test envs) |
| `ed358593` | fix(shim): retry rejected extended spawn without handle/job scoping |

Verified absent at `e4c640dd`: `LayerRef::File { path, layout }` has **no `mount_from` field**
(`e4c640dd:crates/ocx_lib/src/publisher/layer_ref.rs:138-147`), while at `ed358593` it does
(`:141-158`), along with `layout_suffix(layout, mount_from)`, `validate_mount_from`, and the
`:from=` parse arm.

Moving the pointer to `e4c640dd` therefore **deletes the entire shared-wheel-layer design
(Decision D)** the branch's `python_push.rs` is built on. This is not a compile break to patch
around — it removes the capability.

**Two tracks, decide before starting:**

- **Track 1 (unblocked, start now)** — rebase onto `e4c640dd`, strip the mount surface: drop
  `mount_from: None` from `LayerRef` literals, drop the `:from=` arg tails, drop `LayerReuse` from
  `EnvPushReport`. Wheel layers get re-uploaded per app. Correct, just not deduped. Everything else
  in this dossier applies unchanged.
- **Track 2** — land `phase-a-ocx-pr` upstream first, then point at an ocx revision containing
  `f2e4e54e`+`b49515d7`+`b91f7898`, and keep the mount code.

Both tracks share every other break in §3. Start those now regardless of which track wins.

### D-3 — main's `adr_pypi_layer_storage.md` prescribes a redesign of `python_push.rs`.

`ad41264` landed `.claude/artifacts/adr_pypi_layer_storage.md` (Status: **Proposed**, 513 lines) on
main. It explicitly declares:

> Supersedes: the ad-hoc `register_wheel_layers` naming/push behavior on `feat/pypi-mirror`
> (unreleased — no back-compat obligation).

Its six decisions, all targeting branch files that do not exist on main:

1. Tag grammar `sha256.<64-hex>` (period, full hex) — not the branch's bare-hex/`pip-packages/` naming
2. Manifest: empty-config OCI artifact, `artifactType: application/vnd.ocx-mirror.wheel.v1`, one `tar+zstd` layer, no `subject`, no platform field
3. Repository renamed `pypi/<name>:sha256.<hash>`; inline register-before-push in the existing serial Phase-2 loop; no new phase
4. No mirror-side GC
5. Idempotency via tag-exists precheck, fail-safe on transient read error (issue #157 discipline)
6. New additive `Client::push_artifact(&oci::Identifier, …)` called **directly on the authed client** — no subprocess, no metadata sidecar — replacing `register_wheel_layers`'s `ocx package push -m <Bundle>` shelling

Named change sites (`design_spec_pypi_layer_storage.md:198-210`): `ocx_python/src/naming.rs`
(`wheel_reference`, delete `WheelScope`), `pipeline/python_push.rs` (`register_wheel_layers` →
`push_artifact`, drop `platform` param), `command/package/pipeline/push.rs:489-496` (call-site arg
drop), `spec.rs` (remove `wheel_scope` field ~63-67/173-175, tests ~1154-1195),
`pipeline/python_prepare.rs` + `command/package/pipeline/prepare.rs:324,369` (drop `wheel_scope`
threading), `test/tests/test_mirror_mount.py:243` (new tag format).

**Caveat the executor must not skip**: decision 6 depends on `Client::push_artifact`, which does
**not** exist in ocx v0.5.3. The ADR says it lands "in the submodule-bump commit itself." Under
Track 1 that function is unavailable. So D-3 is **gated on D-2** — do not attempt the ADR redesign
until the ocx-side API question is settled. If Track 1 is chosen, keep `register_wheel_layers`'s
subprocess shape for now and file the ADR conformance as follow-up work.

### D-4 — Correction: `TargetError → 69` is **not** stale.

The task brief asked whether the branch's `TargetError → 69` is stale against ocx 0.5.3's exit-75
remap. It is not. These are two distinct exit-code spaces:

1. **ocx-mirror's own process exit code** — `src/error.rs`, `kind_exit_code()`. On main today
   `TargetError => ExitCode::Unavailable` = **69** (`origin/main:src/error.rs:59`). The branch's
   mapping is identical. Nothing to change.
2. **The spawned `ocx` child's exit code** — inspected by `push_exit_is_transient`. This is what
   0.5.3 changed: 75 = `TempFail` (retry), 69 = deterministic (do not retry).

The branch must adopt the *predicate* narrowing (§1, `0f1cd93`), not touch its own error mapping.

---

## 1. Main commit catalog — all 50, with interaction class

**Classes**: (a) no-touch · (b) mechanical, replay as-is · (c) semantic overlap, branch must adopt ·
(d) conflicts with a branch decision, resolve toward main.

Note: under **D-1**, class (b) collapses to "nothing to do" — main's hunks land at their own paths
and the branch no longer moves them. The (b) entries are retained so the executor can verify
nothing was dropped.

### Cluster A — bin_scan / declared binaries / variants / libc lint (2026-07-29, 19 commits)

| SHA | Subject | Class | Notes |
|---|---|---|---|
| `c9fa944` | feat(mirror): bin_scan derives the published binaries claim | **(c)** | New `src/spec/bin_scan.rs` (93 ln): `BinScanMode{Off,Auto,Verify}` (default `Off`), `.scans()`. `MirrorSpec.bin_scan` (`spec.rs:81`), `VariantSpec.bin_scan` / `EffectiveVariant.bin_scan` (`variant.rs:38,63`), `MirrorTask.bin_scan` (`mirror_task.rs:36`). `ExpectedMetadata` + `render`/`adopting_binaries_from` (`orchestrator.rs:83-140`), `MetadataPlan`/`metadata_plan_for` (`orchestrator.rs:166,185`), split `package::extract`/`package::bundle` (`package.rs:27,47`). **Adoption**: the fields must exist on the shared `MirrorSpec`/`VariantSpec` for archive specs; env specs gate them off the same way they already gate `metadata:`/`assets:`/`variants:` (`is_env` dispatch, branch `spec.rs:334-364`). |
| `426b1f4` | test(mirror): register the crypto provider in the bin_scan test | (b) | test-only, `orchestrator.rs` |
| `818cd7c` | docs(mirror): warn about bare `${installPath}` PATH scan | (a) | doc |
| `0dcef74` | test(mirror): pin a variants spec to the golden corpus | (c) | `+1` to `NATIVE_FIXTURES` in `generate/ci.rs`; new `tests/fixtures/mirror-variants.yml` + `tests/golden/mirror-variants.txt`. Branch rewrote `ci.rs` to 64% — re-apply the array entry by hand. |
| `00cba2c` | chore(deps): bump external/ocx onto a fetchable commit | (a) | pointer only |
| `bd7e01d` | feat(mirror): reject a bin_scan whose metadata gives the scan nowhere to look | **(c)** | `MetadataConfig::validate_scannable` (`metadata_config.rs:53-88`), `has_scan_target` (`:98-104`), call site `spec.rs`. Natural no-op for env specs (they reject `metadata:` outright); adopt verbatim for archive specs, gated as main does (`spec.rs:262-281`). |
| `871a274` | docs(mirror): bare-`${installPath}` now rejected, not warned | (c) | error-text tweak riding on `bd7e01d` |
| `77c190c` | docs(mirror): name the asymmetric-archive case | (a) | doc |
| `ba77bfa` | fix(generate): infer repo root from the git repository, not the spec set | **(c)** | `generate/ci.rs` +106/-4. Walks up to the enclosing `.git` instead of the spec paths' common ancestor; fixes doubled `script:` paths and `extends:` resolution for a spec in a subdirectory. Branch rewrote this file — relocate the logic by hand. |
| `8af8910` | test(generate): cover the script-doubling symptom separately | (c) | test for `ba77bfa`, same file |
| `5ec786f` | fix(mirror): the spec owns a declared binaries claim, everywhere | **(c)** | `reject_empty_scan` (`orchestrator.rs:735-747`); `adopting_binaries_from` cements "adopt only when absent" (`:116-139`). **This is the invariant env packages should copy**: `python_push.rs`'s console_scripts-derived `binaries` is a *declared* claim — never silently rewritten from what's published. |
| `64bb97d` | perf(plan): keep the digest short-circuit when the spec declares binaries | **(c)** | `settled_by_digest` (`plan.rs:502-505`): `let complete = !bin_scan.scans() \|\| expected.binaries().is_some();`. Download-free path. If env prepare always populates `binaries`, env `plan`/`patch` inherit the fast path for free. |
| `2fa3622` | fix(mirror): round-2 — verify can fail again, patch refuses layout changes | (c) | `patch.rs` `layout_refusal`/`layout_unchanged`; declared-list carve-out narrowed to `auto` only; `reject_empty_scan` extended to the resume path. `strip_components` is `None` on both sides for a wheel-composed package → benign no-op for env. |
| `f23f05c` | test(mirror): make two born-green round-2 tests discriminate | (c) | mutation hardening for `2fa3622` |
| `deb872f` | chore: drop a debugging probe | (a) | removes `.probe_df.txt` |
| `5ad4554` | feat(packaging): declare ocx-mirror's interface binary explicitly | **(a)** | `packaging/metadata.json` gains `"binaries": ["ocx-mirror"]`. Unaffected by `ocx_python` — it is a compile-time library, never a published OCX package. |
| `984415e` | ci(publish): fix the push sidecar and announce into ocx-sh/index | **(a)** | `oci-publish.yml` +62, `release.yml` +1. Drops the broken `push --metadata packaging/metadata.json` (SOURCE sidecar, no platform → exit 65); `push --announce-file` per leg; one `ocx package announce --tags-from-file --index-repo ocx-sh/index`; both gated behind an `ocx package announce --help` capability probe (`oci-publish.yml:143-148`). **Not coupled to package kind** — hardcodes the single logical package `ocx/mirror`. Env push needs **no** announce work. |
| `8a8e8e4` | build(deps): bump vendored ocx to main for the libc lint | (a) | pointer, prerequisite for `d549125` |
| `d549125` | feat(mirror): check the declared libc against the packaged binaries | **(c) — highest-value adoption** | See §1.1. |
| `1419077` | fix(mirror): round-2 — strict variant keys, honest resume docs | **(c) — live regression on the branch** | See §1.2. |

#### 1.1 — libc lint (`d549125`): no representational conflict, only a wiring gap

Lives in `external/ocx/crates/ocx_lib/src/package/libc_lint.rs`, wrapped at
`src/pipeline/orchestrator.rs:769-795` (`check_declared_libc`), called from `prepare_task` at
`orchestrator.rs:693` — after the bin scan, before the sidecar is written.

It walks regular files under the interface `PATH` directories the metadata declares, reads each
ELF's `PT_INTERP` (`read_elf_libc`, `libc_lint.rs:329-374`), classifies the loader
(`classify_interpreter`, `:396-407`: `ld-musl*` → Musl, `ld-linux*`/`ld.so*`/`ld64.so*` → Glibc),
and compares against the declared set decoded from the platform's `os.features`
(`declared_libcs`, `:295-300`, via `LibcFlavor::from_os_feature_tag`).

Scope: Linux and `Platform::Any` only (`checks_declared_libc`, `:72-81`) — macOS and Windows are
excluded by construction. Mirror-side gate: `MirrorSpec.libc_lint: bool` (`spec.rs:100-101`,
default `true` via `default_true` at `:197-199`), per-variant override
`VariantSpec.libc_lint: Option<bool>` (`variant.rs:40`), threaded to `MirrorTask.libc_lint`
(`mirror_task.rs:40`), checked at `orchestrator.rs:770`.

**The branch's `os.features` libc axis is exactly the vocabulary this lint consumes.** The wire tag
is `libc.glibc`/`libc.musl` (`ocx_lib/src/oci/host_capabilities.rs:150-154`, `os_feature_tag`), and
`declared_libcs` parses those straight out of `Platform::Specific.os_features` — which is precisely
what `457e20d` produces. **This is an adoption target, not a conflict.**

Concrete adoption: `python_prepare.rs`/`python_push.rs` must call
`ocx_lib::package::libc_lint::check_declared_libc(content_dir, &metadata, &platform)` against the
composed env content tree before writing the sidecar, mirroring `orchestrator.rs:769-795`. A
composed env's `bin/pythonX.Y` on an interface `PATH` directory is exactly the dynamically-linked
Linux ELF this lint targets, however it got there.

Known and accepted gap (main documents it too, `libc_lint.rs:102-103`): native `.so` extensions
buried in `site-packages/` are reached via a `PYTHONPATH`-style var, not `Modifier::Path`, so they
are **not** inspected. Do not assume the lint catches numpy/cryptography `.so` files.

#### 1.2 — strict variant keys (`1419077`): branch has regressed, restore main's struct

Main's `src/spec/variant.rs:25-41`:

```rust
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VariantSpec {
    #[serde(default)] pub name: Option<String>,
    #[serde(default)] pub default: bool,
    pub assets: AssetPatterns,                      // required, NOT Option
    #[serde(default)] pub metadata: Option<MetadataConfig>,
    #[serde(default)] pub asset_type: Option<AssetTypeConfig>,
    #[serde(default)] pub bin_scan: Option<BinScanMode>,
    #[serde(default)] pub libc_lint: Option<bool>,
}
```

Rules: any key outside that set is a hard parse error naming the offending key; `assets` is
mandatory per variant; everything else inherits from spec level.

Branch state: `crates/ocx_mirror/src/spec/variant.rs:24` has **no `deny_unknown_fields`**, and
`assets: Option<AssetPatterns>` with a runtime `.expect("validated: …")`/`filter_map` skip
(branch `spec.rs:478-505`).

**Resolution favoring main**: take main's `variant.rs` verbatim. The branch's `Option<AssetPatterns>`
change is unnecessary — `457e20d` already makes env sources reject `variants:` outright, so no
variant ever exists on an env spec and `assets` can stay type-level required. The branch's only
legitimate delta is the doc comment explaining the env-source rejection; keep that, drop the type
change. **Verify during execution**: if some env code path genuinely needs a variant without
assets, that is a design smell to report, not to accommodate.

### Cluster B — ghcr publish / ocx 0.5.0 / containers[].setup / generate pins (07-31 → 08-02, 14 commits)

| SHA | Subject | Class | Notes |
|---|---|---|---|
| `b777220` | ci(publish): publish releases to ghcr.io/ocx-sh/ocx/mirror | (b) | `oci-publish.yml` +48/-11. New `workflow_call` inputs `physical_namespace` (default `""`), `announce` (bool, default `false`); registry allowlist `dev.ocx.sh\|ghcr.io`; identifier `${REGISTRY}/${physical_prefix}ocx/mirror:${VERSION_TAG}`; `--annotation org.opencontainers.image.source`; `setup-ocx` 0.3.7→0.5.0. |
| `e9108be` | ci(publish): (release.yml side) | (b) | `release.yml` +14/-4. `registry: ghcr.io`, `physical_namespace: ocx-sh`, `announce: true`, `permissions: {contents: read, packages: write}`, `github.actor`/`GITHUB_TOKEN` replace `REGISTRY_USER/TOKEN`. |
| `95287c4` | chore(deps): vendor ocx v0.5.0 | (a) | pointer `41eab65`→`ad24829`; superseded by `fddd1cb` |
| `cdd71cc` | release: v0.5.0 | (d)→main | `Cargo.toml` version. Under D-1 there is no conflict — root `Cargo.toml` keeps `[package] version`. |
| `1ca8865` | fix(ci): relock the toolchain for ocx 0.5.0 | (a) | `ocx.lock` `lock_version 1→3`; `verify.yml:124-129` `setup-ocx` 0.3.7→0.5.0. **These two must move together or `ocx pull` exits 78.** |
| `9431372` | fix(generate): bump container-leg ocx to v0.5.2 | (c) | `const OCX_CONTAINER_CLI_TAG` (see below) |
| `982321b` | feat(generate): pin setup-ocx to the renderer ocx version | **(c)** | Adds `fn ocx_cli_version() -> &'static str { OCX_CONTAINER_CLI_TAG.trim_start_matches('v') }` (`ci.rs:1099-1101`) and threads a `{OCX_CLI_VERSION}` placeholder into all 9 `setup-ocx` steps across `workflow.yml`, `describe.yml`, `patch.yml`, `announce-from-registry.yml`, `verify-generated.yml`. 7 goldens regenerate. Branch's `ci.rs` is 64% — re-apply by hand. |
| `62f9fff` | feat(spec): accept containers[].setup | **(c)** | `ContainerConfig.setup: Option<Vec<String>>` (`Option` not `Vec`, so empty≠absent). `validate_container_setup(key, container, errors)` (`spec.rs:778-820`) rejects empty list, blank command, multi-line command. Called from `validate_platforms` at `spec.rs:930`. |
| `338b163` | fix(spec): reject unknown platform and container fields | **(c) — cleared as safe** | `#[serde(deny_unknown_fields)]` on exactly two structs: `ContainerConfig` (`platforms_config.rs:29`) and `PlatformConfig` (`:61`). `ExcludeEntry` is not covered (pre-existing gap). **The branch adds NO new YAML field under `platforms:`/`containers:`** — its libc axis rides entirely in the platform *key string* (`"linux/amd64+libc.musl"`), parsed by `Platform::FromStr`. No rejection risk. |
| `5865464` | feat(ci): build a leg's container image from setup | **(c)** | `MatrixLeg.container_dockerfile`, `render_setup_dockerfile(image, shell, setup)`, `any_container_setup(legs)`, matrix key `container_dockerfile: \|`, `{CONTAINER_SETUP_ENV}` placeholder in `workflow.yml`'s env block. Renders an **inline per-leg `docker build`** inside the existing container prelude (no separate job/file): `printf '%s' "${OCX_CONTAINER_DOCKERFILE}" \| docker build --platform … -t "${OCX_SETUP_TAG}" -`, gated by `docker image inspect`, then `CONTAINER_IMAGE="${OCX_SETUP_TAG}"`. |
| `38e3e47` | docs(mirror-yml): document containers[].setup and containers[].id | (a) | doc + one `subsystem-mirror.md` table row |
| `080b4af` | fix(spec): reject a trailing-backslash setup command | (c) | Rule: `command.trim_end().ends_with('\\')` — checked *after* `trim_end()` deliberately (docker continues on a backslash that is the last non-whitespace char). Error: `"…setup[{index}] must not end with a backslash; it would continue into the next RUN instead of ending the command"`. Test `validate_rejects_a_trailing_backslash_setup_command`, parametrized over trailers `["", " "]`. |
| `f4264f4` | chore(task): port interactive release:prepare from ocx | (b) under D-1 | `BUMP` becomes a required enum (`auto\|patch\|minor\|major`); `VERSION=x.y.z` pins and wins. Tail is `cargo set-version "{{.NEXT_VERSION \| trimPrefix "v"}}"` with no `-p`. **Under D-1 this stays correct as-is** (root package unambiguous); under the workspace conversion it breaks. Also `release.yml:42` `grep -m1 '^version' Cargo.toml` — likewise fine under D-1. |
| `7286ff1` | release: v0.5.1 | (a) | version ceremony |

**libc ↔ containers on main** (all sites): `infer_libc_from_image(image) -> &'static str`
(`spec.rs:623-629`, Alpine basename → `"musl"`, else `"gnu"`); `libc_feature(family)`
(`spec.rs:636-638`, `"musl"→"libc.musl"`, else `"libc.glibc"`); cross-check at `spec.rs:894-928`
comparing the parsed platform key's `os_features` against `libc_feature(infer_libc_from_image(…))`
— mismatch is a hard validation error naming both; `platform_slug()` (`spec.rs:652-668`) appends
sorted/deduped `os_features` so `+libc.musl`/`+libc.glibc` legs get distinct slugs. In `ci.rs`:
`MatrixLeg.container_libc` feeds `OCX_TRIPLE="${OCX_ARCH}-unknown-linux-${{ matrix.container_libc }}"`
(`ci.rs:1152`) which selects the static ocx release URL (`:1160`). **JUnit gating**:
`JUNIT_FILE="junit/junit-${VERSION}-${{ matrix.platform_slug }}-${{ matrix.container_id }}.xml"`
(`ci.rs:1217`) — the slug encodes `+libc.*`, so JUnit results are implicitly libc-discriminated and
`pipeline push` looks them up by that same pair. The branch duplicates this as
`container_libc_for_image()` in its own `platforms_config.rs` — **delete the duplicate, use main's
`spec::infer_libc_from_image`.**

**Version pinning is a hardcoded renderer constant**, not env/cargo-metadata:
`const OCX_CONTAINER_CLI_TAG: &str = "v0.5.3";  // renovate: datasource=github-releases depName=ocx-sh/ocx`
at `src/command/package/pipeline/generate/ci.rs:1093`. It is **independent of the `ocx_lib`
submodule version** (`env!("CARGO_PKG_VERSION")` separately supplies `{OCX_MIRROR_VERSION}`). One
pins the ocx CLI the generated matrix installs; the other is the renderer's own crate version. The
branch's submodule bump does **not** require touching `OCX_CONTAINER_CLI_TAG` — but the branch's
rewritten `ci.rs` must still carry the constant and the `{OCX_CLI_VERSION}` wiring forward.

**Template drift**: `describe.yml`, `verify-generated.yml`, `workflow.yml` each drifted well beyond
this cluster — multi-spec templating (`{TRIGGER_PATHS}`/`{SPEC_SOURCE}`/`{WORKFLOW_SUFFIX}`),
`{REGISTRY_AUTH_STEPS}`, permissions placeholders, announce outputs, metadata-drift filtering — all
predating or paralleling it. `patch.yml` and `announce-from-registry.yml` are entirely new files.
The executor needs main's **current** content as the merge target, not this cluster's delta. The
branch edited three of these (90%/63%/85% similarity) — treat as a full manual reconciliation.

**CI single-crate assumptions** (moot under D-1, listed for completeness):
`.github/actions/build-rust/action.yml:46` (`cargo build --release --target=…`), `:52`
(`cargo zigbuild …`), `.github/workflows/build-matrix.yml:100` (`cargo xwin build …`) — all
unscoped. Under the workspace conversion each would additionally cross-compile `ocx_python` for
every target. Under D-1: no change needed.

### Cluster C — push retry / exit-75 / ocx 0.5.3 / acceptance isolation (08-02 → 08-03, 16 commits)

| SHA | Subject | Class | Notes |
|---|---|---|---|
| `853ab44` | fix(pipeline): make declared binaries executable in archive bundles | (b) | `ensure_declared_binaries_executable(content_dir, binaries)` (`package.rs:101-193`), called from `orchestrator.rs:685` between `bin_scan::resolve_binaries` (`:674`) and `check_declared_libc` (`:693`). Walks `content_dir`, matches basenames against declared `Binaries`, skips already-executable, chmods 0755. **Gap worth an issue, not a rebase task**: never runs for wheel-composed env packages (`python_prepare::compose_env` does not call `package::extract`). |
| `5bd1648` | fix(pipeline): retry transient push failures up to max_retries | (c) | Superseded in-cluster by `0f1cd93`/`075086f` — adopt the *final* state. Removes never-read `ConcurrencyConfig.max_pushes`; test `a_spec_that_still_sets_max_pushes_keeps_parsing` pins that a stale key still parses (no `deny_unknown_fields` on that struct). |
| `c71a508` | chore(claude): bugfix plan artifacts for #51 and #50 | (a) | Later annotated "superseded" by `ad41264` — do not resurrect pre-narrowing versions. |
| `c403c64` | chore(agents): bootstrap hex swarm memory | (b) | `.agents/memory/hex.md`, `.gitignore` narrowed to `.agents/worktrees/`, `CLAUDE.md` Product section |
| `fddd1cb` | chore(deps): bump ocx submodule to v0.5.3 | **(d) — see D-2** | `ad24829`→`e4c640d`; `Cargo.lock` `ocx_lib` 0.5.0→0.5.3. No `.rs` changes in this commit itself. |
| `0f1cd93` | fix(pipeline): retry only exit-75 push failures per ocx 0.5.3 | **(c)** | `push_exit_is_transient` narrowed from `{Unavailable, TempFail}` to `TempFail` only (`push.rs:904`): `matches!(code, Some(code) if code == ExitCode::TempFail as i32)`. `OCX_CONTAINER_CLI_TAG` → `v0.5.3` (`ci.rs:1093`). 8 goldens regenerate for the version line. |
| `6dd88ec` | chore(claude): note ocx#266 landing | (a) | |
| `28274a8` | fix(pipeline): never chmod through a symlink or a hard link | **(b) — security-critical** | `package.rs:172`: `if entry.file_type().is_symlink() \|\| entry.nlink() > 1 { continue; }`, on a `tokio::fs::symlink_metadata` stat (`:163-171`, not the link-following `metadata()`). Closes a real archive escape: self-referential parent symlinks (`a`→`.`) let a declared name pass the purely-lexical `join_under_root` while landing outside the tree; hard links escape identically and are invisible to `is_symlink()`. **Treat as a merge checkpoint — silently dropping this reopens the bug.** |
| `075086f` | fix(pipeline): raise the push timeout to a hang backstop | **(c)** | `pub(crate) const PUSH_TIMEOUT: Duration = Duration::from_secs(3600)` (`push.rs:876`, was 900s), applied inside `push_once` (`:958`) as `tokio::time::timeout(timeout, cmd.output())` (`:975`) over a child spawned with `.kill_on_drop(true)`. `jitter` ±10% from clock nanoseconds (`:931-945`). **`patch::republish` (`patch.rs:336`) calls `push_once(ocx_binary, &args, PUSH_TIMEOUT)` directly, no retry ladder** — the precedent for a second push call site. |
| `edefbc9` | docs(mirror-yml): concurrency and the exec-bit note | (b) | additive doc merge |
| `ad41264` | chore(claude): sync design records with what shipped | **(d) — see D-3** | Adds `adr_pypi_layer_storage.md` (513 ln), `design_spec_pypi_layer_storage.md` (286 ln), `research_pypi_layer_storage.md` (115 ln), `handover_container_setup_hook.md` (206 ln, unrelated to pypi). Marks `max_pushes` never-implemented in `adr_mirror_source_generators.md`/`adr_ocx_mirror.md`. |
| `f3b71a1` | chore(agents): record the acceptance-registry collision | (a) | precursor note to `c49c422` |
| `2801b3c` | test(pipeline): pin the cost of the hardlink heuristic | (b) | Test `an_in_tree_hardlink_pair_is_skipped_too` (`package.rs:396-445`) documenting that `nlink > 1` is a heuristic, not a containment check: an in-tree hardlink pair is skipped too. Currently unreachable (the extractor rejects relative hardlink linknames first) — a deliberate tripwire. Do not let it change silently. |
| `c49c422` | fix(test): isolate acceptance registry from sibling compose projects | **(c) — not yet adopted on the branch** | See §1.3. |
| `c36167c` | docs: drop nonexistent pytest `--no-build` flag | (c) trivial | Branch still has it at `CLAUDE.md:53` and `.claude/agents/worker-tester.md:64`. |
| `d200b72` | release: v0.5.2 | (a) | |

#### 1.3 — acceptance-registry isolation (`c49c422`)

```diff
# test/docker-compose.yml
+# Dedicated project name and host port: sibling repos' compose projects
+# default to `test` with a registry on :5000, and the harness would silently
+# reuse whatever answers there (zot GCs untagged manifests; registry:2 keeps
+# them — the eviction tests depend on the latter).
+name: ocx-mirror-test
+
 services:
   registry:
     image: registry:2
     ports:
-      - "5000:5000"
+      - "5001:5000"
```

`test/conftest.py`: both `pytest_sessionstart` and the `registry()` fixture change
`os.environ.get("REGISTRY", "localhost:5000")` → `"localhost:5001"`.
`.github/workflows/verify.yml` service port `5000:5000`→`5001:5000`. `CLAUDE.md` and
`.claude/agents/worker-tester.md` doc mentions updated. **`test/src/helpers.py` needed no change** —
`start_registry(registry: str)` takes the address as a parameter.

**Branch is entirely unadopted**: `ed26b9d:test/docker-compose.yml` has no `name:` and maps
`"5000:5000"`; `ed26b9d:test/conftest.py` defaults to `:5000` at both sites;
`ed26b9d:.github/workflows/verify.yml:113` still `- 5000:5000`; `CLAUDE.md:21` and
`worker-tester.md:61` still say `:5000`.

The branch's new modules already consume the `registry: str` fixture rather than a hardcoded port,
so they inherit isolation automatically once `conftest.py` is fixed — **no code change in the new
test files**. Three carry stale prose (`test_mirror_libc.py:9`, `test_mirror_mount.py:17`,
`test_mirror_pypi.py:23` say "the real `:5000` registry fixture"); update for accuracy.

#### 1.4 — Retry machinery: the exact extraction the branch needs

`invoke_push` (`src/command/package/pipeline/push.rs:1015-1069`) is **private, one caller**
(`Push::execute` at `:204`). It does two separable things:

```rust
async fn invoke_push(spec, platform, target_ref, bundle_path, cascade) -> Result<PushReport, String> {
    let ocx_binary = resolve_ocx_binary()?;
    let bundle = bundle_path.to_str().ok_or_else(…)?;
    let annotations = crate::annotations::build_annotations(&spec.annotations);
    let args = build_push_args(platform, target_ref, &[bundle], None, &annotations, cascade)?;
    // ---- everything below is generic over `args` ----
    let budget = spec.concurrency.max_retries;
    let total = budget.saturating_add(1);
    let mut attempt = 1u32;
    loop { match push_once(&ocx_binary, &args, PUSH_TIMEOUT).await { … } }
}
```

The loop body is coupled to `&MirrorSpec` only for `spec.concurrency.max_retries` and `spec.name`
(log line). `build_push_args` builds single-bundle **archive**-push args — not the multi-layer
`--cascade --new -m META LAYERS…` shape `python_push.rs` needs.

**Prescription** — extract the loop, do not duplicate it:

```rust
pub(crate) async fn push_with_retry(
    ocx_binary: &Path,
    args: &[String],
    budget: u32,
    label: &str,        // "{spec.name}" — for the warn line
    target_ref: &str,
    platform: &str,
) -> Result<PushReport, String>
```

`invoke_push` becomes: resolve binary + build args + `push_with_retry(...)`. Then:

- **`python_push::invoke_env_push`** (primary publish path) → calls `push_with_retry` with its own
  arg vector and `spec.concurrency.max_retries`. It *should* retry.
- **`:latest` alias push (`ed26b9d`)** → follow the `patch::republish` precedent: call
  `push_once(&ocx_binary, &args, PUSH_TIMEOUT)` directly, single attempt. It is a cheap
  dispatch-triggered re-tag, and the branch already treats its failure as best-effort
  (`log::warn!`, no error propagation).

Constants the branch needs, all `pub(crate)` and same-crate-visible under D-1:
`PUSH_TIMEOUT` (`:876`), `push_once` (`:958`), `build_push_args` (`:825`),
`PUSH_RETRY_BACKOFF_BASE` = 1s, `PUSH_RETRY_BACKOFF_MAX` = 30s, `push_retry_delay` (`:946`),
`push_exit_is_transient` (`:904`). `max_retries` source: `ConcurrencyConfig`
(`src/spec/concurrency_config.rs:16-18`, `#[serde(default = "default_max_retries")]`, default 3).

#### 1.5 — Main's full exit-code map (`origin/main:src/error.rs:52-70`)

| Variant | ExitCode | # |
|---|---|---|
| `SpecInvalid` | `DataError` | 65 |
| `SpecNotFound` | `NotFound` | 79 |
| `ExecutionFailed` | `Failure` | 1 |
| `SourceError` | `Unavailable` | 69 |
| **`TargetError`** | **`Unavailable`** | **69** ← `error.rs:59`, identical to the branch |
| `SpecUsageError` | `UsageError` | 64 |
| `RendererDrift`, `JunitParseError`, `RunSummaryError`, `PlanError` | `DataError` | 65 |
| `TemplateError` | `IoError` | 74 |
| `WebhookUnavailable` | `Unavailable` | 69 |
| `WebhookPermissionDenied` | `PermissionDenied` | 77 |

The branch adds `PylockError` → `DataError` (65) and `PypiError` → `DataError` (65). Both are
additive and correct; keep them.

---

## 2. Branch commit re-analysis — the 5, decomposed

### `e56c137` — feat(ocx_python): Python wheel → OCX packaging library
26 files, **+4463/−0**. Pure addition: `crates/ocx_python/` — `lock.rs`(348), `platform.rs`(905),
`select.rs`(714), `naming.rs`(281), `repack.rs`(622), `compose.rs`(735), `collide.rs`(133),
`error.rs`(48), `lib.rs`(55), plus a fixture corpus (`generate.py`, `pylock/*.toml`, real `.whl`
binaries).

Design claims: pylock→OCX is a pure translation library; repack is byte-reproducible (golden
sha256); collision detection is first-class and separate from composition.

- **Survives verbatim**: everything except `compose.rs`'s `base_platform()` (break #3, §3) and the
  `Bundle{..}` literal (break #1). Main has no `ocx_python` at all.
- **Re-express**: those two literals only.
- **Dies**: nothing. Note `platform.rs` already shed 266 lines of dead L2 encode surface in
  `457e20d` — that deletion is internal to the branch, nothing to port.

### `5da32e2` — feat(mirror): pylock source type, env pipeline & container CI
96 files, **+6076/−409**. **Two unrelated concerns in one commit.** Decomposition (reconciles
exactly):

| Bucket | Files | +/− | Disposition |
|---|---|---|---|
| **Pure relocation (R100, zero delta)** | 48 | 0/0 | **DELETE under D-1.** All of `src/{annotations,command.rs,discord,filter,junit,main,normalizer,resolver*,run_summary,version_platform_map}.rs`, most of `command/package/**`, `pipeline/{download,mirror_result,mirror_task,orchestrator,package,progress,verify}.rs`, most `spec/*_config.rs`+`target.rs`, `source/{github_release,url_index}.rs`, 8 `tests/fixtures/*.yml` |
| **Renamed + edited** | 15 | **+2305/−276** | Keep the *edits*, drop the rename. `generate/ci.rs`(517/119), `plan.rs`(482/8), `prepare.rs`(518/1), `command/package/pipeline/push.rs`(346/59), `spec.rs`(346/43), `templates/{describe,verify-generated,workflow}.yml`, `spec/source.rs`(38/0), `error.rs`(21/0), `sync.rs`(6/0), `pipeline.rs`(3/0), `pipeline/push.rs`(1/2), `source.rs`(1/0), `describe.rs`(1/1) |
| **Genuinely new** | 19 | +3059/−47 | Keep. `pipeline/{ocx_cli.rs(66),python_prepare.rs(717),python_push.rs(303)}`, `source/pylock.rs`(233), `spec/python_config.rs`(70), `spec/variant.rs`(122), 3 fixtures, 4 `.claude/artifacts/*.md`(1175), 4 pylock-negative fixtures, `test_mirror_pylock.py`(170) |
| **Build/doc plumbing** | 10 | +712/−86 | **Mostly delete under D-1**: `.licenserc.toml`, `taskfiles/{release,rust}.taskfile.yml`, `.github/workflows/release.yml`, most of `Cargo.toml`. Keep: `README.md`(27/0), `docs/reference/mirror-yml.md`(86/5), `CLAUDE.md`(5/2), `subsystem-mirror.md`(8/2) |

Design claims: pylock resolves exactly one version (unlike GithubRelease/UrlIndex's many-per-run) —
`Source::pylock_app_name()` / `Source::is_env()`; env plan/prepare/push logic extracted into
lock-agnostic builders so `pypi` can reuse them.

### `39f6816` — feat(mirror): source.type pypi
44 files, **+6053/−190**. Single concern, no relocation noise. New `source/pypi.rs`(327),
`pipeline/lock_derive.rs`(805, uv-driven PEP 751 derivation), `spec/python_config.rs` +324;
`run_summary.rs` gains `LayerReuse{mounted,uploaded,verified}` (additive, `#[serde(default)]`).
Bumps `external/ocx` `c04f3697`→`63c79e3b` (layer-mount).

Claims: universal-tag lock derivation is interpreter-less; a derived lock is named
`pylock.*.toml`, distinct from a committed one; `uv` resolution failure is a data error (65), not
unavailable; wheel layers are content-addressed, registered once per run, `:from=`-mounted by every
leg (Decision D).

- **Survives**: `source/pypi.rs`, `lock_derive.rs`, `spec/python_config.rs` — all new files.
- **Re-express**: `run_summary.rs`'s `LayerReuse` must be re-inserted into main's rewritten file.
  **Under Track 1 (D-2), `LayerReuse` is dead** — v0.5.3's `PushReport` has no `layers` field, so
  the counters would be permanently zero. Drop it under Track 1.
- **Dies**: the `external/ocx` bump — superseded by D-2's decision.

### `457e20d` — feat(mirror)!: env wheels: platform keys — libc as os.features (BREAKING)
30 files, **+2042/−1484**. Replaces the `variants:` axis for env sources with a top-level `wheels:`
map keyed by an OCI platform string with an optional `+libc.glibc`/`+libc.musl` suffix. New
`spec/wheels.rs`(339): `pub struct WheelPatterns { pub filters: HashMap<Platform, Option<Vec<String>>> }`
with a custom `Deserialize` parsing each YAML key through `Platform::from_str`. Deletes
`mirror-pylock-musl-variant.yml`, adds `test_mirror_libc.py`(330).

`os_features` is never computed from wheel contents — it is declared verbatim by the maintainer in
the `wheels:` key, and the mirror's push `-p` flag publishes the `+libc.*`-suffixed platform key.

**BREAKING means**: an env spec (`pylock`/`pypi`) using `variants:` is hard-rejected; `wheels:` is
mandatory and `variants:` mandatorily absent, enforced source-aware in `MirrorSpec::validate`.
Non-env sources are unaffected.

- **Survives**: `spec/wheels.rs` as a new type.
- **Re-express**: the `Platform::Specific` destructure (breaks #4-#7, §3) — the field set *and* the
  feature grammar changed at v0.5.3. `spec/platforms_config.rs`'s `container_libc_for_image()`
  duplicate must be **deleted** in favor of main's `spec::infer_libc_from_image`.
- **Dies**: the deleted musl-variant fixture stays deleted; the branch's `variant.rs` rewrite dies
  in favor of main's (§1.2).

### `ed26b9d` — feat(push): alias newest non-semver env version under :latest
2 files, **+41/−1**. In `execute_pylock_push`:

```rust
manifests.sort_by(|a, b| match (Version::parse(&a.version), Version::parse(&b.version)) {
    (Some(a), Some(b)) => a.cmp(&b),
    _ => a.version.cmp(&b.version),
});
let newest_version = manifests.last().map(|m| m.version.clone());
```

Semver-parseable versions compare parsed; otherwise raw string comparison; "newest" = last after
the mixed sort. The alias fires per-platform only when `!cascade && is_newest`, where
`cascade = Version::parse(version).is_some()` — i.e. **only non-semver versions get the alias, and
only when also overall-newest**. It calls `python_push::invoke_env_push(platform_str, &latest_ref,
…, false)` — the same helper as the primary push, with `latest_ref = "{registry}/{repository}:latest"`.
Best-effort: `Ok` appends `"latest"` to `cascade_tags` (dedup-guarded), `Err` only warns.

- **Survives**: the logic is self-contained.
- **Re-express**: physically must be re-hunked — `push.rs` is 4320 lines on main vs 2648 on the
  branch tip. Route it through `push_once(…, PUSH_TIMEOUT)` per §1.4.
- **Dies**: its `external/ocx` bump (`63c79e3b`→`ed358593`, a Windows shim fix) — superseded.

---

## 3. ocx_lib v0.5.3 adaptation list

`M` = main's already-adapted shape is the reference. `L` = **lost** side-branch API (needs D-2 Track 2,
or removal under Track 1).

| # | Branch file:line | Current | Required | Cause |
|---|---|---|---|---|
| 1 | `crates/ocx_python/src/compose.rs:340-346` | `Bundle { version, strip_components, env, dependencies, entrypoints }` | add `binaries: None` | `Bundle` gained `pub binaries: Option<Binaries>` |
| 2 | `crates/ocx_mirror/src/pipeline/python_push.rs:313-319` | same `bundle::Bundle { … }` | add `binaries: None` | same |
| 3 | `crates/ocx_python/src/compose.rs:526-533` | `Platform::Specific { os, arch, variant: None, os_version: None, os_features: Vec::new(), features: None }` | `Platform::Specific { os, arch, variant: None, os_features: Vec::new() }` | `006b24dc` / `adr_platform_model_unification.md` D2 — `os_version` **and** `features` deleted |
| 4 | `crates/ocx_mirror/src/spec/wheels.rs:69-79` | `let Platform::Specific { os, variant, os_version, os_features, .. }` | drop `os_version` from the pattern | same |
| 5 | `crates/ocx_mirror/src/spec/wheels.rs:80-84` | `if variant.is_some() \|\| os_version.is_some()`; msg `"OCI variant/os_version segments are not supported"` | `if variant.is_some()`; reword — a 4-segment key is now a hard `PlatformError::InvalidFormat` at parse time (`FromStr` rejects `parts.len() > 3`) | same |
| 6 | `crates/ocx_mirror/src/spec/wheels.rs:255-267` | test asserts error contains `"variant/os_version"` | update to the new message | same |
| 7 | `crates/ocx_mirror/src/spec/wheels.rs:266-273` | test key `"linux/amd64+libc.glibc+libc.musl"` expects `"at most one libc feature"` | **behavioural break**: v0.5.3's grammar is `+a,b` (comma-list; `+`/`,`/`/`/`%` percent-escaped). `+libc.glibc+libc.musl` now parses as ONE feature named `libc.glibc+libc.musl`, so `os_features.len() > 1` never fires — the "unsupported platform feature" arm does. Change the key to `"linux/amd64+libc.glibc,libc.musl"` | same |
| 8 | `crates/ocx_mirror/src/pipeline/push.rs:38-42` **L** | `LayerRef::File { path, layout, mount_from: None }` | `LayerRef::File { path, layout }` — `mount_from` does not exist at v0.5.3 | side-branch `f2e4e54e` unmerged |
| 9 | `crates/ocx_mirror/src/pipeline/push.rs:46,65` **M** | `.push_cascade(info.clone(), &layers, versions, None)` | `.push_cascade(vec![info.clone()], &layers, versions, None, canonical_tag, annotations)` | `Vec<Info>` fan-out + `canonical_tag: bool` + `annotations: &BTreeMap<String,String>` |
| 10 | `crates/ocx_mirror/src/pipeline/push.rs:76` **M** | `.push(info, &layers, None)` | `.push(vec![info], &layers, None, canonical_tag, annotations)` | same |
| 11 | `crates/ocx_mirror/src/pipeline/push.rs:25-32` **M** | `push_and_cascade(publisher, info, bundle_path, cascade, cascade_versions, variant)` | add trailing `annotations: &BTreeMap<String, String>`; `let canonical_tag = true;` | main's signature |
| 12 | `crates/ocx_mirror/src/pipeline/orchestrator.rs:540` | `push::push_and_cascade(publisher, info, bundle_path, task.cascade, cascade_versions, task.variant.as_ref())` | thread the new `annotations` argument | knock-on of #11 |
| 13 | `crates/ocx_mirror/src/pipeline/python_push.rs:156` **L** | `args.push(format!("{layer_str}:from={}", layer.wheel_repository))` | **runtime break, not compile**: v0.5.3's `LayerRef::from_str` has no `from=` key — the `:` tail parses as `strip=`/`prefix=` → `MalformedLayout`, **exit 64 on every env push** | `b49515d7` unmerged |
| 14 | `crates/ocx_mirror/src/pipeline/python_push.rs:377-379,405-413` **L** | tests assert `:from=` tails | must change with #13 | same |
| 15 | `crates/ocx_mirror/src/pipeline/python_push.rs:50-54` **L** | `pub layers: LayerReuse` on `EnvPushReport` | `#[serde(default)]` means no parse failure, but v0.5.3's `PushReport` **has no `layers` field** — counters permanently zero. It gained `canonical_tags_written: Vec<String>` instead | same |
| 16 | `crates/ocx_mirror/src/annotations.rs` (whole file) **M** | dead-code stub returning `HashMap`, `#[allow(dead_code)]` | replace wholesale with main's `src/annotations.rs` (274 ln): `build_annotations(&BTreeMap) -> BTreeMap`, `GITHUB_SERVER_URL`/`GITHUB_REPOSITORY`/`GITHUB_SHA` allowlist — it is the source of the `annotations` arg in #9/#10 | main already adapted |
| 17 | `docs/reference/mirror-yml.md:37` | `<os>/<arch>[/<variant>][/<os_version>][+libc.<flavor>...]` | drop `os_version`; feature list is `+a,b` not `+a+b` | doc drift |
| 18 | `.claude/rules/subsystem-mirror.md:38` | `os/arch[/variant][/os_version][+libc.<flavor>]` | same fix | doc drift |

### Reference call shapes from `origin/main` (verified against `e4c640dd`)

`git show origin/main:src/pipeline/push.rs` lines 41-99:

```rust
let layers = [LayerRef::File { path: bundle_path.to_path_buf(), layout: LayerLayoutSpec::default() }];
let canonical_tag = true;   // matches `ocx package push`'s own default
publisher.push_cascade(vec![info.clone()], &layers, cascade_versions.clone(), None,
                       canonical_tag, annotations).await?;
publisher.push(vec![info], &layers, None, canonical_tag, annotations).await?;
```

`Bundle` with `binaries` — from ocx's own `crates/ocx_lib/src/publisher.rs:266` test at `e4c640dd`:

```rust
Metadata::Bundle(Bundle {
    binaries: None,
    version: bundle::Version::V1,
    strip_components: None,
    env: metadata_env::Env::default(),
    dependencies: …,
    entrypoints: …,
})
```

`Binaries: TryFrom<BTreeSet<BinaryName>>`, `BinaryName: TryFrom<&str>/TryFrom<String>`
(`crates/ocx_lib/src/package/metadata/binary.rs:143,171,104`). **`None` = undeclared,
`Some([])` = "publisher asserts zero binaries" — deliberately distinct wire states.** `None` is the
only safe mechanical default for `ocx_python`.

### Signature deltas, quoted

```rust
// crates/ocx_lib/src/publisher.rs   ed358593 → e4c640dd
- pub async fn push(&self, info: Info, layers: &[LayerRef], build_meta: Option<&str>) -> Result<PushOutcome>
+ pub async fn push(&self, infos: Vec<Info>, layers: &[LayerRef], build_meta: Option<&str>,
+                   canonical_tag: bool, annotations: &BTreeMap<String, String>) -> Result<PushOutcome>

- pub async fn push_cascade(&self, info: Info, layers: &[LayerRef],
-                           existing_versions: BTreeSet<Version>, build_meta: Option<&str>) -> Result<PushOutcome>
+ pub async fn push_cascade(&self, infos: Vec<Info>, layers: &[LayerRef],
+                           existing_versions: BTreeSet<Version>, build_meta: Option<&str>,
+                           canonical_tag: bool, annotations: &BTreeMap<String, String>) -> Result<PushOutcome>

  pub struct PushOutcome {
      pub manifest_digest: oci::Digest,
      pub cascade_tags: Vec<String>,
-     pub layer_counts: oci::LayerCounts,   // side-branch only, never upstream
+     pub canonical_tags: Vec<String>,      // sha256.<hex> safety-net tags
  }
```

```rust
// crates/ocx_lib/src/oci/platform.rs
  Specific { os, arch, variant: Option<String>,
-            os_version: Option<String>,
             os_features: Vec<String>,
-            features: Option<Vec<String>> }

- from_image_index / from_manifest : Result<Vec<Self>>  →  Vec<Self>  (no longer fallible)
- can_run / lock_key / base_lock_key / supported_set / all_supported     REMOVED
+ candidate_from_descriptor(&native::ImageIndexEntry) -> Option<Self>
+ with_os_feature(&self, feature: &str) -> Self
+ is_compatible / compatibility_score / select_best / enum Selection<T>  (free fns in oci::)
// Display/FromStr grammar:  os/arch[/variant][+feat,feat]   was  os/arch[/variant][/os_version][+feat+feat]
```

**The branch uses none of the removed `Platform` methods** (grep-verified) — only the two field
deletions bite.

### Verified UNCHANGED — no edits needed

`c04f3697…e4c640dd` touches 131 files under `ocx_lib/src`; none of these is among them:

`archive` (`Archive`, `ExtractOptions`) · `compression` (`CompressionOptions`, `default_threads`) ·
`cli.rs` + `cli/progress` (`DataInterface`, `Printer`, `ProgressManager`, `Spinner`, `ColorMode`,
`LogLevel`, `LogSettings`, `ProgressMode`, `Cell`) · `package/bundle.rs` (`BundleBuilder`) ·
`package/info.rs` (`Info`, byte-identical) · `oci/digest.rs` · `oci/identifier.rs` (`Identifier`,
`PinnedIdentifier`) · `oci/annotations.rs` · `utility/string_ext.rs` ·
`package/metadata/visibility.rs` · `package/metadata/env.rs` (`EnvBuilder::{new,with_path,
with_constant,build}` at identical lines 109/127/136/141).

Also unchanged in shape: `Entrypoints::new`, `Dependencies::new`, `Metadata::Bundle`,
`Client::{fetch_manifest, list_tags}`, `ClientBuilder::from_env`, `env::var`,
`Publisher::{new, client, list_tags, ensure_auth}`, `oci::Manifest`,
`native::Platform { os_version, os_features }` (the OCI **wire** type — `target_registry.rs:186-191`
needs no edit; main constructs it identically at `target_registry.rs:364`).

Additive-only, no call-site impact: `ClientError::{ShortBlobRead, InvalidImageIndex,
RegistryTransient}`, `Error::MetadataBlobTooLarge`, `DependencyError::{DuplicateRepository,
TooManyDependencies}` (all `#[non_exhaustive]`), `ModifierKind: FromStr`, `ClientBuilder::ssrf_guard`.

`sync.rs:290` is byte-identical to main's and compiles; `Platform::candidate_from_descriptor` is
now the one-liner equivalent — optional cleanup only.

### Cargo manifests

- **`[patch.crates-io]` is correct where it is.** Main has it at root `Cargo.toml:80-82`; the branch
  put it at its workspace root. Same two entries as ocx v0.5.3's own table
  (`external/ocx/Cargo.toml:16-18`): `oci-client` → `external/ocx/external/rust-oci-client`,
  `docker_credential` → `external/ocx/external/docker_credential`. Under **D-1** it simply stays in
  main's root `Cargo.toml` untouched. Neither fork requirement moved (`oci-client "0.17"`,
  `docker_credential "1.3"`).
- **`futures` is missing on the branch** — ocx v0.5.3 and main both have `0.3.32`; the branch has
  none. Main's `src/command/package/pipeline/plan.rs:11` uses
  `futures::stream::{self, StreamExt, TryStreamExt}` (concurrent tag observation), which the rebase
  pulls in. **ADD `futures = "0.3.32"`.**
- **`reqwest` stays at 0.12** — ocx v0.5.3 uses `0.13` with `default-features=false, ["rustls"]`;
  main uses `0.12` with `["rustls-tls-webpki-roots-no-provider","json"]`. Main's `Cargo.lock`
  already carries **both** (`0.12.28` for the mirror, `0.13.4` via `ocx_lib`) plus
  `webpki-root-certs 1.0.7` on one `rustls 0.23.40`. This is accepted upstream state, **not drift**
  — `reqwest`/`rustls`/`octocrab`/`url` are mirror-owned since v0.4.1 per CLAUDE.md.
- **All other shared deps match ocx exactly**: `toml 1.1.2`, `thiserror 2.0.18`, `tar 0.4.46`,
  `zstd 0.13 ["zstdmt"]`, `zip 8.6 no-default ["deflate"]`, `tokio`, `clap`, `anyhow`, `serde`,
  `serde_json`, `serde_yaml_ng`, `schemars`, `chrono`, `regex`, `sha2`, `hex`, `tracing`, `tempfile`.
- **Version**: branch is `0.4.0`, main is `0.5.2`. Take main's.
- **`Cargo.lock` must be regenerated** — the submodule move rewrites the `ocx_lib` path-dep tree.

---

## 4. Rebase strategy

### 4.1 Replay order

```
origin/main (d200b72)
  │
  ├─ R1  feat(ocx_python): Python wheel → OCX packaging library     [= e56c137, +1 line in root Cargo.toml]
  ├─ R2  fix(deps): adapt ocx_python to ocx_lib v0.5.3              [breaks #1, #3]
  ├─ R3  feat(mirror): source.type pylock — env pipeline            [= 5da32e2 MINUS the relocation]
  ├─ R4  feat(mirror): source.type pypi — discovery & lock derive   [= 39f6816]
  ├─ R5  feat(mirror)!: env wheels: platform keys, libc os.features [= 457e20d, breaks #4-#7]
  ├─ R6  feat(push): alias newest non-semver env version as :latest [= ed26b9d]
  └─ R7  refactor(push): extract push_with_retry; wire env pushes   [§1.4]
```

**Do not** replay the workspace conversion as a first mechanical step. Under D-1 it does not happen
at all. `R1` adds one line to main's root `Cargo.toml`:
`ocx_python = { path = "crates/ocx_python" }` — cargo makes it a member automatically because it is
a path dep inside the workspace directory.

**Do not** replay `5da32e2`, `39f6816`, `ed26b9d`'s `external/ocx` pointer bumps. Under D-2 Track 1
the pointer stays at main's `e4c640dd`. Under Track 2 it moves once, at the end, to a revision
containing the merged layer-mount commits.

`R2` is separated from `R1` deliberately so `ocx_python` lands as the reviewed artifact it was, with
the v0.5.3 adaptation as its own reviewable diff.

`R7` is separated because it is a refactor of main's code, not branch content — Two Hats.

### 4.2 Conflict-by-conflict resolution guide

| File | Conflict | Resolution |
|---|---|---|
| `Cargo.toml` | Branch converts to virtual workspace; main keeps `[package]` at root | **Take main's entirely.** Add exactly two lines: `ocx_python = { path = "crates/ocx_python" }` under `[dependencies]`, and `futures = "0.3.32"`. |
| `Cargo.lock` | Both modified | Delete both sides' version, regenerate with `cargo build --locked=false` then commit. Verify the fork source afterwards (§5). |
| `src/spec.rs` | Branch 83% rewrite vs main's `bin_scan`/`libc_lint`/`validate_scannable`/`validate_container_setup` additions | Take main's file as the base. Re-insert the branch's env-source arms by hand: `PythonConfig`/`WheelPatterns` fields, `is_env()` dispatch in `validate_assets_or_variants`, the `wheels:`-required / `variants:`-forbidden gate. Keep main's `infer_libc_from_image`/`libc_feature`/`platform_slug` — delete the branch's duplicates. |
| `src/spec/variant.rs` | Branch dropped `deny_unknown_fields`, made `assets` Optional; main added `bin_scan`/`libc_lint` | **Take main's verbatim** (§1.2). Port only the branch's doc comment about env sources rejecting `variants:`. |
| `src/spec/platforms_config.rs` | Branch adds `container_libc_for_image()`; main adds `deny_unknown_fields` + `ContainerConfig.setup` | Take main's. **Delete** the branch's `container_libc_for_image` — it duplicates `spec::infer_libc_from_image` (`spec.rs:623-629`). |
| `src/command/package/pipeline/push.rs` | 4320 (main) vs 2648 (branch) lines; main added retry ladder, `PUSH_TIMEOUT`, jitter, announce-file | Take main's. Re-insert `execute_pylock_push` and the `:latest` alias block as new functions. Do **not** hand-merge — re-express. |
| `src/command/package/pipeline/prepare.rs` | Branch 51% rewrite; main added the bin_scan/libc window | Take main's. Re-insert the env-prepare branch, wiring `check_declared_libc` per §1.1. |
| `src/command/package/pipeline/plan.rs` | Branch rewrote; main added `settled_by_digest` + concurrent tag observation via `futures` | Take main's. Re-insert env-plan arms; ensure env prepare always populates `binaries` so `settled_by_digest` short-circuits (§1, `64bb97d`). |
| `src/command/package/pipeline/generate/ci.rs` | 4429 (main) vs 2340 (branch) lines | Take main's. Re-insert env CI-template rendering. **Verify main's `OCX_CONTAINER_CLI_TAG`, `ocx_cli_version()`, `{OCX_CLI_VERSION}` wiring, `render_setup_dockerfile`, `any_container_setup`, `MatrixLeg.container_dockerfile`/`container_libc`, `NATIVE_FIXTURES` array, and `ba77bfa`'s git-root inference all survive.** |
| `templates/{describe,verify-generated,workflow}.yml` | Both edited | Take main's (they carry multi-spec, announce, patch, `{OCX_CLI_VERSION}`, `{CONTAINER_SETUP_ENV}`). Re-apply only genuinely env-specific placeholders. |
| `src/pipeline/push.rs` | Branch 91%; main added annotations/canonical_tag threading | Take main's; apply breaks #8-#12. |
| `src/error.rs` | Branch 78% | Take main's; add only `PylockError`/`PypiError` → `DataError`. `TargetError → 69` is already correct on both (D-4). |
| `src/annotations.rs` | Branch has a dead stub; main has the real 274-line implementation | Take main's wholesale (break #16). |
| `src/run_summary.rs` | Branch adds `LayerReuse` | **Track 1: drop `LayerReuse`** (v0.5.3 `PushReport` has no `layers`). Track 2: re-insert into main's file. |
| `tests/fixtures/*.yml` (8 shared) | **Branch copies still carry `ocx_mirror: release_tag: v0.7.2`** | **Take main's content verbatim.** Main's `OcxMirrorConfig` is `deny_unknown_fields` with only `rev` (`origin/main:src/spec/ocx_mirror_config.rs:22-29`) — a leftover `release_tag:` is now a **hard `SpecInvalid` (exit 65)**, so those 8 fixtures do not merely render differently, they fail to parse. Affects `mirror-{all-test-kinds,full-platforms,generator-source,minimal,multi-container,r3-discord-url,rejects-ocx-install,windows-arm64}.yml`. |
| `tests/golden/*.txt` | New from main, 8 files | Under D-1 they stay at `tests/golden/`. Regenerate after every spec-serialization change (§5). |
| `test/conftest.py` | Both insert fixtures at the same anchor (after `ocx_binary()`) | Straight interleave: keep main's `:5001` change in `pytest_sessionstart` and `registry()`; keep main's new `mirror_binary`, `mirror`, `unique_mirror_repo`, `asset_server`, `WebhookCapture`, `webhook_server`, `pipeline_spec`; add the branch's `real_ocx_binary`. |
| `test/src/helpers.py` | Branch-only (`push_stub_ocx_package`) | Take the branch's wholesale — main did not touch it. |
| `test/src/mirror_runner.py` | Main-only (`cwd=str(self.temp_dir)`) | Take main's wholesale. Invocation is a resolved binary path, not `cargo run` — unaffected by anything here. |
| `test/docker-compose.yml`, `.github/workflows/verify.yml` | Branch unadopted | Take main's (`name: ocx-mirror-test`, `5001:5000`). |
| `.claude/rules/subsystem-mirror.md` | **Overlapping hunks at merge-base lines 39-41** — main rewrites `spec/concurrency_config.rs`…`spec/ocx_mirror_config.rs` rows and adds `spec/announce_config.rs`; branch inserts `spec/python_config.rs`/`spec/wheels.rs` over lines 32-41 | Manual interleave of both row sets. Also the "Spec Format (YAML)" summary line is a same-line conflict: main adds `bin_scan`/`libc_lint`, branch adds the env `wheels:` sentence — combine both. Fix the platform grammar (break #18). Frontmatter `paths:` needs no change under D-1. |
| `CLAUDE.md` | Main adds a `## Product` section + `:5000`→`:5001`; branch edits the "Dependency model" bullet | No line-level collision. Take both. **Layout table needs no workspace fix under D-1** — but add a `crates/ocx_python` row. |
| `docs/reference/mirror-yml.md` | **`## platforms` section: both replace the same `!!! warning "Container legs are currently rejected"` box** | Main's sentence wins (platform *key*, "same form `assets` uses — including the `+libc.<flavor>` suffix") and main's box (containers now supported). Fold the branch's libc test-leg content in as additional prose, not a replacement. Fix the platform grammar (break #17). |
| `docs/reference/cli.md` | Both edited | Additive merge. |
| `.licenserc.toml`, `taskfiles/{rust,release}.taskfile.yml`, `.github/workflows/release.yml` | Branch changed all four for the workspace | **Revert to main's versions.** Under D-1 none of these needs changing. |
| `renovate.json` | Main-only | Take main's. Under D-1 the `^src/...` anchors stay valid. |
| `external/ocx` | Pointer conflict | **D-2 decision.** Track 1: keep `e4c640dd`. Track 2: bump once at the end. |

### 4.3 Alignment steps

- **`ocx.lock` V3 + `setup-ocx` pin** — main's `1ca8865` moved `ocx.lock` to `lock_version 3` and
  `verify.yml:124-129` to `setup-ocx@0.5.0` together. **These two must stay in lockstep or
  `ocx pull` exits 78.** Take main's both; do not partially resolve.
- **`OCX_CONTAINER_CLI_TAG`** — main is at `v0.5.3` (`ci.rs:1093`). Renovate owns this literal on
  its own schedule; the branch's submodule bump does not require touching it. Ensure the constant,
  `ocx_cli_version()`, and the `{OCX_CLI_VERSION}` placeholder wiring all survive the `ci.rs`
  reconciliation.
- **exit-75 narrowing** — adopt `push_exit_is_transient` = `TempFail` only (`push.rs:904`). Leave
  `src/error.rs`'s `TargetError → 69` alone (D-4).
- **D5 / unified platform model** — breaks #3-#7 and #9-#12 in §3.
- **Layer-mount (Decision D)** — breaks #8, #13-#15, gated on D-2.

### 4.4 Test-suite adaptation list

1. **Strip `ocx_mirror: release_tag:` from all 8 shared fixtures** (take main's content) — otherwise
   every spec-load test fails with exit 65 before any golden comparison runs.
2. **Copy main's 7 new fixtures**: `mirror-{container-libc,container-mixed,container-setup,
   ghcr-announce,push-retry,two-platform-announce,variants}.yml`. Three
   (`ghcr-announce`, `two-platform-announce`, `variants`) are referenced by `NATIVE_FIXTURES` and
   have goldens.
3. **Regenerate all 8 goldens** after the spec changes land: `UPDATE_GOLDEN=1 cargo test`
   (`ci.rs:3673`, env var read at `:3681`). No `--bless`, no `insta`, no task target.
4. **`conftest.py` interleave** per §4.2.
5. **Collapse the duplicate `mirror_binary` fixtures** — `test_mirror_libc.py`,
   `test_mirror_mount.py`, `test_mirror_pylock.py`, `test_mirror_pypi.py` each define a
   module-local `mirror_binary` that shadows main's new conftest-level one. Not an error (pytest
   prefers the closest scope) but dead duplication; use main's shared fixture.
6. **Update `:5000` → `:5001` prose** in `test_mirror_libc.py:9`, `test_mirror_mount.py:17`,
   `test_mirror_pypi.py:23`.
7. **Adopt `c36167c`** — drop `--no-build` from `CLAUDE.md:53` and
   `.claude/agents/worker-tester.md:64`.
8. **Do not resurrect** `mirror-pylock-musl-variant.yml` (deleted by `457e20d`, correctly).

---

## 5. Verification checklist for the executor

Each item is a command plus the specific thing it must prove.

- [ ] `cargo fmt --check` — clean before every commit.
- [ ] `task verify` — full gate green.
- [ ] `cargo tree -i oci-client --locked | head -1 | grep -F 'external/ocx/external/rust-oci-client'` —
      proves `[patch.crates-io]` still resolves to the fork. **CI asserts this**; a dropped patch
      table silently resolves unpatched crates.io releases.
- [ ] `cargo build --locked` — proves `Cargo.lock` was regenerated and committed.
- [ ] `UPDATE_GOLDEN=1 cargo test` then `git diff --stat tests/golden/` — inspect **every** golden
      delta and confirm each is explained by an intended spec-serialization change. An unexplained
      golden diff is a regression, not a refresh.
- [ ] `cargo test` (no `UPDATE_GOLDEN`) — goldens byte-identical on a clean run.
- [ ] Grep `rg 'release_tag' tests/fixtures/` returns **nothing** — the exit-65 fixture trap is closed.
- [ ] `rg 'deny_unknown_fields' src/spec/variant.rs` hits — `1419077`'s strict-key regression is
      repaired.
- [ ] `rg 'nlink\(\) > 1' src/pipeline/package.rs` hits — `28274a8`'s archive-escape guard survived
      the merge.
- [ ] `rg 'ExitCode::TempFail' src/command/package/pipeline/push.rs` hits and
      `rg 'ExitCode::Unavailable as i32' src/command/package/pipeline/push.rs` does **not** —
      the exit-75 narrowing landed.
- [ ] `rg 'container_libc_for_image' src/` returns **nothing** — the duplicate libc inference was
      deleted in favour of `spec::infer_libc_from_image`.
- [ ] `rg 'os_version' src/spec/wheels.rs crates/ocx_python/src/compose.rs` returns **nothing** —
      the D5 field deletions are complete.
- [ ] **Renderer container features**: render every container fixture and diff against its golden —
      `mirror-container-setup.yml` must emit the `container_dockerfile:` block-scalar matrix key and
      the `docker build` prelude; `mirror-container-libc.yml` and `mirror-container-mixed.yml` must
      emit distinct `platform_slug`s carrying `+libc.musl`/`+libc.glibc` and matching
      `OCX_TRIPLE=…-unknown-linux-{musl,gnu}`.
- [ ] `cd test && uv run pytest tests/ -v` — full acceptance suite (note: **no `--no-build` flag**,
      it does not exist).
- [ ] `cd test && uv run pytest tests/test_renovate_managers.py -v` — proves every
      `renovate.json` customManager still matches a real file.
- [ ] `cd test && uv run pytest tests/test_mirror_libc.py tests/test_mirror_pylock.py tests/test_mirror_pypi.py tests/test_mirror_mount.py -v` —
      env pipeline acceptance. Under D-2 Track 1, `test_mirror_mount.py` will need its
      layer-reuse assertions relaxed or the module skipped; **state which, do not silently delete.**
- [ ] `docker compose -f test/docker-compose.yml config | grep -E 'name:|5001'` — registry isolation
      adopted.
- [ ] `git diff origin/main --stat -- src/command/package/pipeline/patch.rs src/command/package/pipeline/announce.rs src/spec/bin_scan.rs src/spec/announce_config.rs` —
      **must be empty.** These four files came from main and no branch commit should touch them.
      A non-empty diff means the rebase moved or mangled them.
- [ ] `git log --oneline origin/main..HEAD` — exactly the 7 replay commits from §4.1, no
      "Checkpoint", no merge commits.

### Report-back requirements

The executor must explicitly state, not assume:

1. Which D-2 track was taken, and if Track 1, exactly what was stripped.
2. Whether D-3 (the `adr_pypi_layer_storage.md` redesign) was attempted or deferred, with reason.
3. Every golden that changed, and why.
4. Any test that was skipped or weakened, with the reason and a follow-up issue reference.

---

## Appendix — open items deliberately not resolved here

- **D-3 ADR conformance** is gated on D-2 and needs an owner decision. It is a redesign of
  `python_push.rs`, not a rebase mechanic.
- **Exec-bit for wheel-composed packages**: `ensure_declared_binaries_executable` never runs on the
  env path (`python_prepare::compose_env` does not call `package::extract`). Console-script shims
  synthesized from wheels get no mode normalization. This is a **pre-existing gap, not a rebase
  regression** — file an issue, do not fix it inside the rebase.
- **libc lint blind spot**: native `.so` files inside `site-packages/` are reached via a
  `PYTHONPATH`-style var, not `Modifier::Path`, so `check_declared_libc` does not inspect them.
  Main documents and accepts this (`libc_lint.rs:102-103`). Do not assume coverage.
