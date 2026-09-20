# System Design: `source.type: cargo` — native per-platform crate builds in the mirror pipeline

**Companion to:** [`adr_cargo_source_kind.md`](./adr_cargo_source_kind.md)
**Owner:** Architect → `/hex-plan` → Builder
**Status:** Proposed
**Date:** 2026-09-17

This spec defines component contracts for **v1** (C4 levels 1–3, code level only where the ADR names a seam); phase 2–4 items are named where they attach. It does not assign files to work packages — that is `/hex-plan`'s job. Every path:line is against ocx-mirror @ 7aad053 (v0.6.1).

---

## L1 — Context

```
                  crates.io sparse index          <dl> host (static.crates.io)
                  index.crates.io/<p>/<crate>     <crate>-<ver>.crate (cksum'd)
                         ▲   (plan: versions,            ▲   (plan: manifest pre-read, in-process)
                         │    cksum, features)           │   (prepare: cargo's own fetch, --index pinned,
                         │                               │    cksum bound to the plan after the build)
 operator ──edits──► mirror.yml ──► ocx-mirror ──────────┴──────────────────────► target OCI registry
 (repo owner)        cargo: block   plan │ prepare×N │ push                        ocx.sh/<repo>:<V>_<TS>
                                         │           │                             + cascade + annotations
                                    GitHub-hosted    │
                                    or generic CI    │ push holds the ONLY credential
                                    runners ×6       │
                                                     ▼
                                              ocx clients (`ocx package install`) — see
                                              "built by ocx-mirror", never "mirrored"
```

Actors: the operator (authors the spec, owns the runners and the push credential); crates.io index + `dl` host (source of versions and tarballs — the in-scope attacker per `security-threat-model.md`); the CI system (trusted execution environment, GitHub or generic); the target registry; ocx clients.

## L2 — Containers (the four commands + runner legs)

```
 discover (ubuntu-24.04)          prepare ×(versions × platforms)               test ×(platforms × containers)  push (ubuntu-24.04)
 ┌────────────────────┐   plan    ┌──────────────────────────────┐  bundle-V-P  ┌───────────────┐    ┌──────────────────────┐
 │ pipeline plan      │──artifact─►│ runs-on: platforms.<P>.runner│──artifact───►│ ocx package   │    │ pipeline push --plan │
 │  · sparse index    │           │ NO secrets, contents:read,   │  + metadata  │   test        │    │  · plan_sha256 ✓     │
 │  · .crate pre-read │           │ persist-credentials:false    │  + manifest  │ (native /     │    │  · manifest sha256 ✓ │
 │  · plan.json v4    │           │ pipeline prepare             │              │  container)   │    │  · plan complete ✓   │
 │  · plan_sha256 out │           │   --platform P --plan        │              │ junit-V-P-C   │    │  · JUnit AND gate    │
 └────────────────────┘           │ · cargo install --locked     │              └───────┬───────┘    │  · ocx package push  │
        │                          │   --index … --target …       │                      │            │    --cascade --sign  │
        │  legs = [{version,       │ · window: scan→chmod→libc→   │                      │            │    --annotation …    │
        │  platform, slug, runner}]│   sidecar→bundle             │                      └───────────►│  · run-summary.json  │
        └─────────────────────────►└──────────────────────────────┘                                   └──────────┬───────────┘
                                                                                                                  ▼ notify
 ═══════════════════════════════ secret boundary: nothing left of this line holds a credential ══════════════════════════════
```

The line is the ADR's compensating control: `discover`, every `prepare` leg and every `test` leg run with zero registry/signing/announce/webhook secrets. `push` and `notify` are the only credential holders, unchanged from today (`workflow.yml:158-217`, `:264-283`). `discover` is also the one trusted job that finishes before any upstream code runs — its `plan_sha256` output is the only leg-authenticated channel `push` has.

## L3 — Components and data flow

### Modules (new ★ / changed △), v1

| Module | Role |
|---|---|
| ★ `src/source/cargo.rs` | SSRF-floored sparse-index client (`list_versions` → `Vec<VersionInfo>`, assets empty), in-process `.crate` pre-read (`tar`+`flate2`+`toml`; `CrateManifest { bins: Vec<BinName>, required_features, rust_version, has_lock }`), feature closure, `expected_bins(features, no_default) -> Vec<BinName>`; `CrateName`/`BinName` newtypes; 65/69 classification |
| ★ `src/pipeline/cargo_build.rs` | The `cargo install` subprocess (argv builder over newtypes, `--index`, env deny-list scrub, timeout, `kill_on_drop`, stderr tail with control bytes stripped), post-build cksum bind against `$CARGO_HOME/registry/cache`, `.crates2.json`/`.crates.toml` strip, `BuildFacts`, explicit `target/` + `content/` removal after the bundle |
| △ `src/spec/source.rs` | `Source::Cargo { crate_name, index }`; Cargo arm of `Source::validate` beside the Pypi arm (`:245-262`: https-only, no userinfo); `is_build()` beside `is_env()` (`:120-134`) |
| ★ `src/spec/cargo_config.rs` | `CargoConfig { toolchain, features, no_default_features, bins, timeout_seconds }` — all defaulted, `deny_unknown_fields` (so `rebuild`/`profile` are refused until their phase) |
| △ `src/spec.rs` | `cargo: Option<CargoConfig>` field; third validation arm in `validate_assets_or_variants` (`:470-517`): `variants:` rejected for cargo (phase 2 lifts it via `validate_variants`, `:548-572`) |
| △ `src/spec/platform_keys.rs` | `rust_target_triple(&Platform) -> Result<&'static str, String>` next to `platform_slug` (`:96-112`) — the same join-key module, because a wrong triple is the same class of bug as a wrong slug |
| △ `src/pipeline/mirror_task.rs` | `acquire: Acquire` replaces `download_url`/`asset_name`/`asset_type`/`verify_config` (`:28-29,41,44`) — refactor commit, output-identical |
| △ `src/pipeline/orchestrator.rs` | `prepare_task` acquisition phase matches on `Acquire` (`:657-706`); the window (`:711-737`) unchanged; `BundleEntry.cargo: Option<BuildFacts>` (`:56-65`), `None` for other kinds so their `manifest.json` is byte-identical; a second `ExpectedMetadata` constructor for the synthesized cargo metadata (bypasses `resolve_metadata`, `:630-634`) |
| △ `src/command/package/pipeline/plan.rs` | cargo branch beside the env branches (`:315-342`); `PlanCargoEntry`; `schema_version: 4` (`:420`) |
| △ `src/command/package/pipeline/prepare.rs` | `--platform` filter (all kinds); `--plan` required for cargo (64); cargo task construction from the plan with argv-field re-validation (`build_tasks_from_plan`, `:654-726`) |
| △ `src/command/package/pipeline/push.rs` + `push/bundles.rs` | `--plan` required for cargo; `--plan-sha256`; completeness (`missing_bundle`); manifest sidecar check (`manifest_mismatch`/`manifest_missing`); per-version annotation overlay |
| △ `src/command/package/sync.rs`, `check.rs` | `list_upstream_versions` arm (`:304-308`); `sync` refuses cargo (64); `check` works (index list + tag scan) |
| △ `src/error.rs` | `CargoError` (65) in `kind_exit_code` (`:125`) |
| △ `generate/ci.rs`, `ci/matrix.rs`, `templates/workflow.yml` | cargo prepare matrix, toolchain steps, `legs`/`plan_sha256` outputs, `!cancelled()` test gate, permissions, push plan download (§4) |
| △ `Cargo.toml` | `semver`, `tar`, `flate2`, `toml` — four lines copied exactly from ocx `[workspace.dependencies]`; zero new lock entries |

### Data flow: plan → prepare (build) → bundle → push

```
plan (one process, once per run; holds GITHUB_TOKEN — runs no cargo, no upstream code)
  list_versions(index, crate)          ─► VersionInfo{version, assets:{}}          [404→65, net→69]
  for each NEW version (filter_versions, platform_applies):
    config.json dl → guard host (https, no userinfo, same SSRF floor)
    GET .crate, sha256==cksum (64 hex)  ─► CrateManifest (in-process tar+flate2+toml; ≤64 MiB decompressed,
                                            ≤10 000 entries, no path escape, exactly one Cargo.toml)     [→65]
    every [[bin]] name ∈ ^[A-Za-z0-9_-]{1,64}$                                      [→65]
    expected_bins                       ─► non-empty or 65 (lib-only / filtered-to-zero / unknown bins:)
    has_lock, rust_version vs pin       ─► 65 if violated
    PlanVersionEntry{ version:"V_TS", source_version:V, platforms:[K..], assets:[],
                      cargo:{crate,version,index,cksum,toolchain,features,no_default_features,bins,timeout_seconds,targets:{K:triple}} }
  plan.json schema 4; rendered discover also exports plan_sha256 + legs

prepare --version V --plan plan.json --platform K        (one process per leg; --plan required for cargo → 64)
  entry = plan.versions[version==V]; entry.cargo or PlanError(65); re-validate crate/version/bins/toolchain/index/targets
  tasks filtered to --platform keys       [value ∉ spec.platforms → 64; declared key ∉ entry.platforms → warn, no-op]
  prepare_task:
    resume: bundle.tar.xz exists → adopt sidecar binaries, finalize, return           (unchanged, :636-655)
    acquire (CPU permit):
      cargo +<tc> install <crate>@<ver> --locked --index <index> --root content --target <triple> [--no-default-features] [--features a,b] [--bin=x]…
        env = process env − deny-list; kill_on_drop; timeout                        [spawn/timeout/non-zero → ExecutionFailed 1, ≤40-line tail]
      sha256($CARGO_HOME/registry/cache/*/<crate>-<ver>.crate) == plan.cksum          [mismatch → ExecutionFailed 1]
      rm content/.crates2.json content/.crates.toml
    window (unchanged): resolve_binaries(Verify vs expected bins) → chmod declared → libc_lint → sidecar → bundle → drop content/
    rm -r target/ (cargo_build's own cleanup; the prepare path removes only content/, :738)
  {work_dir}/{V}/{slug}/{bundle.tar.xz, metadata.json}; {work_dir}/{V}/manifest.json with BundleEntry.cargo = BuildFacts

flatten (cargo prepare job only) → bundles/bundle-{V}-{slug}.tar.xz, -metadata.json, -manifest.json → artifact bundle-{V}-{slug}

push --bundles-dir --junit-dir --plan plan.json --plan-sha256 HEX     (--plan required for cargo → 64)
  sha256(plan.json) == HEX                                                             [→ PlanError 65]
  enumerate bundles ∪ plan-declared (V,K) → missing_bundle failures
  per cargo (V,K): manifest entry present and sha256(bundle) == entry.sha256          [else manifest_missing / manifest_mismatch]
  JUnit AND-gate (unchanged); ocx package push -p K -i target:V [--cascade iff no failure for V]
      --annotation sh.ocx.mirror.cargo.{crate,version,cksum,features,no-default-features,toolchain[,rustc]}=… --annotation sh.ocx.mirror.rev=…
```

### Window mapping for a build tree (the load-bearing invariant, `subsystem-mirror.md` › Phase 1)

| Step (archive) | Where it reads | Build-tree equivalent | Same code? |
|---|---|---|---|
| extract (`orchestrator.rs:703`) | archive → `content/` | `cargo install --root content` → `content/bin/*` | no — the `Acquire::CargoBuild` arm |
| `bin_scan` (`:711`) | `content/` + metadata `PATH` dirs | `content/bin`, mode `Verify` against the synthesized/declared `binaries` = pre-read expected bins (`spec/bin_scan.rs:25-29`) — the **one** binaries check | yes |
| chmod declared (`:722`) | declared names | cargo already writes 0755; no-op but kept (idempotent) | yes |
| `libc_lint` (`:730`) | each interface binary's `PT_INTERP` vs `os.features` | glibc build → `ld-linux`; musl target → static/no interp → `+libc.musl` | yes |
| sidecar (`:731`) | metadata | same file, `binaries` filled | yes |
| bundle + drop `content/` (`:737-738`) | `content/` | same; `target/` is **not** dropped by this path — `cargo_build` removes it afterwards | yes + one cleanup |

Nothing in the window is re-implemented. The cargo-specific steps (cksum bind, `.crates2.json`/`.crates.toml` strip) run **before** the window so a refused tree never reaches the sidecar — the "refused version leaves nothing publishable" rule holds.

## Code level — the two seams

### `MirrorTask` acquire split (`src/pipeline/mirror_task.rs`)

```rust
pub enum Acquire {
    Download { url: Url, asset_name: String, asset_type: AssetType, verify: Option<VerifyConfig> },
    CargoBuild(CargoBuildSpec),
}
pub struct CargoBuildSpec {
    pub crate_name: CrateName,          // newtype: validated grammar, `Display` is the only way out
    pub version: semver::Version,
    pub index: Url,                     // validated: https, no userinfo
    pub cksum: [u8; 32],                // bound to the fetched .crate after the build
    pub toolchain: String,              // ^[A-Za-z0-9._-]{1,64}$
    pub features: Vec<String>, pub no_default_features: bool,
    pub bins: Vec<BinName>,             // the `--bin=` args and the Verify claim
    pub target: &'static str,           // from rust_target_triple
    pub timeout: Duration,
}
pub struct MirrorTask { version, normalized_version, platform, acquire: Acquire, target, metadata_config,
                        bin_scan, libc_lint, cascade, spec_dir, variant }
```
`prepare_task` (`orchestrator.rs:612`): the download-permit block (`:657-680`) and the extract call (`:693-706`) become `match &task.acquire { Download{..} => <today>, CargoBuild(b) => cargo_build::install(b, &content_dir, &task_dir).await? }`; `archive_path` exists only in the Download arm; `verify::verify`'s `asset_name`/`download_url` arguments (`:670-677`) come from the arm, not the task. The refactor commit lands with every existing test green and no output change.

### Platform key → triple (`src/spec/platform_keys.rs`)

```rust
pub fn rust_target_triple(p: &Platform) -> Result<&'static str, String> {
    let libc = os_feature_libc(p);                  // None | Some("gnu") | Some("musl"); "glibc" → "gnu"
    match (p.os(), p.arch(), libc) {
        (Linux,   Amd64, None | Some("gnu"))  => Ok("x86_64-unknown-linux-gnu"),
        (Linux,   Amd64, Some("musl"))        => Ok("x86_64-unknown-linux-musl"),
        (Linux,   Arm64, None | Some("gnu"))  => Ok("aarch64-unknown-linux-gnu"),
        (Linux,   Arm64, Some("musl"))        => Ok("aarch64-unknown-linux-musl"),
        (Darwin,  Amd64, None)                => Ok("x86_64-apple-darwin"),
        (Darwin,  Arm64, None)                => Ok("aarch64-apple-darwin"),
        (Windows, Amd64, None)                => Ok("x86_64-pc-windows-msvc"),
        (Windows, Arm64, None)                => Ok("aarch64-pc-windows-msvc"),
        _ => Err(format!("platform '{p}' has no supported rust target")),
    }
}
```
Called at validate (every `platforms:` key must map), at plan (`targets` map), and never at prepare (prepare reads the plan). A libc feature on darwin/windows is an error, not ignored.

## 1. `mirror.yml` additions (v1)

| Field | Type | Required | Rule |
|---|---|---|---|
| `source.type` | `"cargo"` | — | discriminant |
| `source.crate` | string | no (default `name`) | `^[A-Za-z0-9_-]{1,64}$`, no leading `-`, not a Windows device name |
| `source.index` | https URL | no (default `https://index.crates.io`) | Cargo arm of `Source::validate` (`spec/source.rs:245-262` shape): https only, no userinfo, no query/fragment |
| `cargo` | block | **no** (every field defaults) | rejected (65) for every other kind |
| `cargo.toolchain` | string | no (`stable`) | `^[A-Za-z0-9._-]{1,64}$` — rustup toolchain name; `+<toolchain>` on argv outranks the crate's `rust-toolchain.toml` |
| `cargo.features` / `no_default_features` / `bins` | list / bool / list | no | the one feature set of v1 |
| `cargo.timeout_seconds` | u32 | no (3600) | per `cargo install` |
| `platforms.<K>` | | **yes** | every K must pass `rust_target_triple`; `runner` as today (`platforms_config.rs:61-63`) |
| rejected for cargo | `assets`, `asset_type`, `variants`, `wheels`, `python`, `verify`, `bin_scan` | | 65, message names the field and the reason |
| rejected elsewhere | `cargo:` | | 65 |
| phase 2 | `variants[].features/no_default_features/bins`, `VariantSpec.assets: Option` | | ADR S5 |
| phase 3 | `cargo.rebuild` | | ADR S7 |

## 2. CLI surface

| Command | Delta |
|---|---|
| `pipeline plan` | cargo branch; `plan.json` schema 4 with `cargo` per entry; 65 `CargoError` on every plan-time refusal, 69 on index/`dl` unavailability. Still side-effect-free (the pre-read is in memory; only `plan.json` is written) |
| `pipeline prepare` | `--platform KEY` (repeatable, all kinds; value ∉ `spec.platforms` → 64); `--plan` required for cargo (64); cargo build; build failures → 1 |
| `pipeline push` | `--plan` required for cargo (64), optional elsewhere; `--plan-sha256 HEX`; completeness + manifest checks; cargo annotations; `run-summary.json` `platforms_failed[].reason ∈ {…, "missing_bundle", "manifest_mismatch", "manifest_missing"}` |
| `pipeline generate ci` | cargo prepare matrix (§4); new golden; existing goldens byte-identical |
| `package check` | works for cargo (index list + tag scan, no build) |
| `package sync` | exit 64 for a cargo spec |

Exit-code contract per `subsystem-mirror.md` › Error Model: 65 = malformed/absent data (crate, manifest, plan), 69 = unavailable (index, `dl`), 1 = a build or tool failure on a leg, 64 = usage (missing `--plan`, bad `--platform` value, `sync`).

## 3. Inter-job artifacts

| Artifact | Producer → Consumer | Content |
|---|---|---|
| `plan` (`plan.json`) + job output `plan_sha256` | discover → prepare legs, **push** (new download; digest verified) | schema 4 |
| `bundle-{V}-{slug}` | one per prepare leg → test, push | `bundle-{V}-{slug}.tar.xz`, `-metadata.json`, `-manifest.json` (the leg's `manifest.json`: `BundleEntry { platform_slug, bundle_path, size_bytes, sha256, cargo: BuildFacts { rustc, target, runner, duration_s } }`) |
| `junit-{slug}-{container_id}` | test → push | unchanged (`workflow.yml:151-156`) |
| `run-summary` | push → notify | unchanged shape + the new reason strings |

`BuildFacts.runner` is the runner **label** (never a hostname — a cron box's hostname would land in run-summary and Discord). Push recomputes `sha256(bundle)` and compares; the check binds bundle ↔ manifest against corruption/omission and is **not** a cross-leg tamper barrier (both travel in the same store; ADR OQ2).

## 4. Rendered workflow (GitHub) — job graph for a cargo spec

```
discover ──► prepare [matrix.include = legs (version × platform), runs-on: matrix.runner]
                     ├ 0.9.100 / linux/amd64        @ ubuntu-24.04
                     ├ 0.9.100 / linux/arm64        @ ubuntu-24.04-arm
                     ├ 0.9.100 / darwin/amd64       @ macos-15-intel
                     ├ 0.9.100 / darwin/arm64       @ macos-15
                     ├ 0.9.100 / windows/amd64      @ windows-2025
                     └ 0.9.100 / windows/arm64      @ windows-11-arm
         ──► test [matrix.include = platforms × containers; versions looped inside each leg, workflow.yml:94-150]
         ──► push ──► notify
```

Discover step additions (rendered only for cargo; `RUNNERS` and `SLUGS` are baked at render time from `platforms.<p>.runner` and `spec::platform_key_slug`, so the workflow stays self-contained and the slug can never drift from the Rust function):

```yaml
          RUNNERS='{"linux/amd64":"ubuntu-24.04","linux/arm64":"ubuntu-24.04-arm", ...}'
          SLUGS='{"linux/amd64":"linux_amd64","linux/amd64+libc.musl":"linux_amd64_libc.musl", ...}'
          echo "legs=$(jq -c --argjson r "${RUNNERS}" --argjson s "${SLUGS}" '[.versions[] | select(.kind != "metadata-drift")
            | .version as $v | .platforms[] | {version: $v, platform: ., platform_slug: $s[.], runner: $r[.]}]' plan.json)" >> "${GITHUB_OUTPUT}"
          echo "plan_sha256=$(sha256sum plan.json | cut -d' ' -f1)" >> "${GITHUB_OUTPUT}"
```

Prepare job for cargo (replaces `workflow.yml:62-92` for this kind only; the workflow top gains `permissions: { contents: read }`, which `push` overrides per job as today):

```yaml
  prepare:
    needs: discover
    if: needs.discover.outputs.has_new == 'true'
    permissions:
      contents: read
    strategy:
      fail-fast: false
      matrix:
        include: ${{ fromJson(needs.discover.outputs.legs) }}
    runs-on: ${{ matrix.runner }}
    env:                                   # matrix values reach shell only through env, never inline
      VERSION: ${{ matrix.version }}
      PLATFORM: ${{ matrix.platform }}
      PLATFORM_SLUG: ${{ matrix.platform_slug }}
    steps:
      - uses: actions/checkout@<pin>
        with:
          persist-credentials: false        # build.rs must not find a token in .git/config
      - uses: ocx-sh/setup-ocx@<pin>
        with: { version: "<OCX_CLI_VERSION>" }
      - uses: actions/download-artifact@<pin>
        with: { name: plan }
      - name: Toolchain
        shell: bash
        run: |
          set -euo pipefail
          rustup toolchain install "<cargo.toolchain>" --profile minimal
          TRIPLE=$(jq -r --arg v "${VERSION}" --arg p "${PLATFORM}" '.versions[] | select(.version==$v) | .cargo.targets[$p]' plan.json | tr -d '\r')
          rustup target add --toolchain "<cargo.toolchain>" "${TRIPLE}"
          case "${TRIPLE}" in *-linux-musl) sudo apt-get install -y musl-tools ;; esac
      - name: Prepare
        shell: bash
        run: |
          set -euo pipefail
          ocx-mirror package pipeline prepare<SPEC_ARG> --version "${VERSION}" --plan plan.json --platform "${PLATFORM}"
          <PREPARE_FLATTEN: bundle, metadata.json, manifest.json>
      - uses: actions/upload-artifact@<pin>
        with:
          name: bundle-${{ env.VERSION }}-${{ env.PLATFORM_SLUG }}
          path: bundles/
          retention-days: 1
```
No `env:` with `secrets.*` anywhere in this job — the golden asserts `grep -c 'secrets\.'` = 0 between `prepare:` and `test:`.

Test job (cargo render): `permissions: { contents: read }` and
```yaml
    if: ${{ !cancelled() && needs.discover.outputs.has_new == 'true' }}
```
Today's `if:` (`workflow.yml:96`) carries no status function → implicit `success()` over `needs: [discover, prepare]` → one red prepare leg skips **every** test leg and starves push's JUnit gate for all platforms, contradicting the per-platform-red contract. Pre-existing for every kind (the per-version prepare matrix hides it); the shared template keeps its line until the standalone fix lands so existing goldens stay byte-identical. Everything else in `test` is unchanged. The push job adds one `download-artifact` (`name: plan`) and `--plan plan.json --plan-sha256 ${{ needs.discover.outputs.plan_sha256 }}`. Notify is byte-identical. Windows legs run the mirror in Git Bash (`shell: bash`) exactly as the test job's Windows legs do today (`matrix.rs:653-657` CR handling applies).

## 5. The equivalent non-GitHub job (GitLab, the "four-line job")

```yaml
plan:     { image: ocx-sh/ocx:0.6.2, script: ["ocx-mirror package pipeline plan --format json > plan.json"], artifacts: { paths: [plan.json] } }
build:    { parallel: { matrix: [ { P: [linux/amd64, linux/arm64] } ] }, tags: ["$P"], needs: [plan],
            script: ["ocx-mirror package pipeline prepare --version \"$V\" --plan plan.json --platform \"$P\"", "<flatten>"], artifacts: { paths: [bundles/] } }
test:     { ... ocx package test per bundle → junit/ ... }
push:     { needs: [build, test], script: ["ocx-mirror package pipeline push --bundles-dir bundles --junit-dir junit --plan plan.json --write-summary run-summary.json"], secrets: [OCX_REGISTRY_TOKEN] }
```
Runner selection is the operator's (`tags:`), toolchain provisioning is the operator's, the four commands are ours. The push secret is declared on `push` only — never on `build` — and `cargo_build`'s deny-list scrub is the backstop if an operator exports it globally. A cron box runs the same four lines sequentially with `--platform` = its own host key; `--target` cross-builds are possible but unsupported in the happy path.

## 6. Testability per component

| Component | Test seam |
|---|---|
| index client | fixture sparse index dir served by the harness (`test/`), unit tests over captured index lines (`features2`, yanked, `rust_version`, non-hex `cksum`); a redirect answer must fail (Policy::none) |
| manifest pre-read | fixture `.crate` tarballs (lib-only, `required-features`, no lock, path-escape entry, duplicate `Cargo.toml`, a 65 MiB decompressed bomb, a `[[bin]]` named `-rf`) under `tests/fixtures/cargo/` |
| feature closure / expected bins | pure unit tests |
| triple table | exhaustive over six keys ± libc; darwin+libc rejected |
| argv builder | never sees a raw string: `CrateName`/`BinName`/`semver::Version` newtypes; a property test that the grammars reject `-x`, `/`, whitespace, `;`, NUL |
| `cargo_build::install` | acceptance only (needs cargo); a `PATH` stub `cargo` script in unit tests for exit-code, timeout, stderr-tail stripping and env-scrub assertions (the `lock_derive` `uv` stub precedent, `OCX_ENV_LOCK`) |
| manifest / plan checks | unit: mismatched sha → `manifest_mismatch`; absent entry → `manifest_missing`; wrong `--plan-sha256` → 65; plan declares 2 platforms, 1 bundle → `missing_bundle`, cascade withheld |
| renderer | golden `mirror-cargo.txt`; existing goldens byte-identical |
| end to end | `test/tests/test_mirror_cargo.py`: local index + local `.crate` + real `cargo` on linux/amd64 through all four commands; lib-only crate → plan 65; cargo `prepare` without `--plan` → 64 |

## 7. What this spec does NOT decide

- Phase 2 variants' exact validation messages (ADR S5 fixes the shape and the three callers).
- The rebuild trigger's plan-side comparison (phase 3, ADR S7) — annotation read via the existing drift scan (`plan/drift.rs`) is the intended seat.
- SLSA provenance predicate contents, the referrer media type, and `cargo-auditable` wiring (phase 4, ADR S8).
- The durable per-platform `rustc` annotation seat (needs an ocx-side manifest-level annotation — ADR OQ1).
- Build caching (`Swatinem/rust-cache` keyed on the crate name) — documented upgrade, not rendered in v1.

## 8. Open items

Carried verbatim from the ADR's *Open questions*: OQ1 toolchain truth (`stable` + durable actual-rustc annotation); OQ2 cross-leg tampering residual (accept for v1 with `plan_sha256`); OQ3 `windows/arm64` in the acceptance bar (keep).

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| 2026-09-17 | Architect (hex-arch-cargo-20260917) | Initial draft |
| 2026-09-17 | Architect (hex-arch-cargo-20260917) | hex-architect Round-1 panel fixes applied (4 Block, 7 High, 17 Warn) |
| 2026-09-17 | Orchestrator (hex-arch-cargo-20260917) | Re-validation residuals applied (3 Warn, 3 Suggest): run-summary clause, `skip_serializing_if`, bare-hex `cksum` + re-validated list, wording |
