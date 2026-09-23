# Research: ocx's Bazel solution and the mirror's cross-module shape

## Metadata

- **Date:** 2026-09-22
- **Domain:** ci-cd
- **Triggered by:** `.agents/discussions/bazel-crate-split.md`
- **Expires:** 2027-03-22

## Direct Answer

ocx (`/home/mherwig/dev/ocx`, read-only checkout) runs Bazel 9.2.0 with `rules_rust`
0.74.0's bzlmod `crate.from_cargo` extension reading its own root `Cargo.toml`/`Cargo.lock`,
17 hand-written `crates/*/BUILD.bazel` files for its own workspace members, and
**auto-generated** `BUILD.bazel` files written by `crate_universe`'s splicer directly into
the working tree of every `[patch.crates-io]` path crate (the three nested submodules under
`external/`). That last mechanism is the load-bearing fact for the mirror's cross-module
question (§F): the mirror's own `external/ocx/crates/*` path dependencies are structurally
identical to ocx's patch-table path crates (both: local-path crates excluded from the
reading Cargo workspace, referenced from the Cargo.lock the `crate.from_cargo` manifest
scans), so the same auto-generation applies — **the mirror does not need to hand-write or
commit BUILD files for `external/ocx/crates/*`; a single crate_universe hub off the
mirror's own Cargo.toml/Cargo.lock will generate them itself, uncommitted, in-place,**
exactly as it already does for ocx's three nested patch submodules today (verified by
commit `3d445fb6`, § B below). **Recommendation: Option 1.** Option 2 (`bazel_dep` +
`local_path_override` consuming ocx's own Bazel targets) is blocked on multiple fronts,
independently sourced in §F: ocx's pinned submodule commit (`191b9324`) predates ocx's own
`MODULE.bazel` (added at `63c1b71f`, six hours later) so there is nothing to override yet;
ocx's `git_override` and `register_toolchains` calls would go inert as a non-root module
per Bazel's own bzlmod docs; and mainline `rules_rust` has no crate-coalescing across two
`crate_universe` hubs (that exists only in the experimental `hermeticbuild/rules_rs` fork),
so two hubs would produce two incompatible copies of every shared third-party crate.

---

### A. `MODULE.bazel`, `.bazelrc`, `.bazelversion`, `BUILD.bazel`, `.bazelignore`

**`MODULE.bazel`** (`/home/mherwig/dev/ocx/MODULE.bazel`, 119 lines, module `ocx` v0.6.2):

- `bazel_dep(name = "rules_rust", version = "0.74.0")`, `rules_shell` 0.8.0,
  `buildifier_prebuilt` 8.5.1.4, `rules_ocx` 0.4.0 (MODULE.bazel:22-28). Every
  `bazel_dep` carries an explicit version by policy (MODULE.bazel:20-21).
- **Rust toolchain** (MODULE.bazel:33-42): `rust = use_extension("@rules_rust//rust:extensions.bzl", "rust")`,
  `rust.toolchain(edition = "2024", versions = ["1.95.0"])`, `use_repo(rust, "rust_toolchains")`,
  `register_toolchains("@rust_toolchains//:all")`. No `platforms` bzlmod module is used
  anywhere in the repo (`grep -rn platform MODULE.bazel .bazelrc BUILD.bazel` — no hits
  besides a prose comment) — the build is Linux-only by CI-lane convention, not by a
  declared platform matrix.
- **`crate.from_cargo`** (MODULE.bazel:52-59):
  ```
  crate = use_extension("@rules_rust//crate_universe:extensions.bzl", "crate")
  crate.from_cargo(
      name = "crates",
      cargo_lockfile = "//:Cargo.lock",
      lockfile = "//:Cargo.bazel.lock.json",
      manifests = ["//:Cargo.toml"],
  )
  use_repo(crate, "crates")
  ```
  **One manifest, the workspace root.** No `crate.annotation()` calls exist anywhere in
  the file (`grep -n annotation MODULE.bazel` — zero matches) and no `crate.spec` path
  overrides. **The `[patch.crates-io]` table in `Cargo.toml` is read implicitly** — `from_cargo`
  shells out to `cargo metadata`/its own splicer against the manifest, which already
  resolves Cargo's own patch table; nothing in `MODULE.bazel` re-declares it. This is
  the mechanism behind §B's finding: three patched path crates get spliced and get
  generated `BUILD.bazel` files without any Bazel-side annotation naming them.
- **Toolchain-tool extension** (MODULE.bazel:66-119): `git_override(module_name = "rules_ocx", commit = "9ced5ffb…")`
  because "`rules_ocx` has no BCR release carrying its Bazel 9 support yet" (MODULE.bazel:70-76,
  explicitly temporary, dropped once a BCR release lands); `ocx = use_extension(...)`,
  `ocx.project(name = "tools", ocx_lock = "//:ocx.lock", ocx_toml = "//:ocx.toml")`,
  `use_repo(ocx, "tools")` — this is how `uv`, `bun`, etc. are exposed as `@tools//:<bin>`
  for `sh_test`/genrule wrappers (see §D), sha256-pinned, never off `PATH`.
- `compatibility_level` is deliberately omitted (MODULE.bazel:8-10: "non-functional from
  Bazel 9.1.0 on… declaring one would encode a guarantee nothing enforces").

**`.bazelversion`**: `9.2.0` (pin authority; `MODULE.bazel`'s own docstring notes Bazel
is pinned by `ocx.lock`'s per-platform digest, `.bazelversion` retained only for
BZL-FLAG-01 / bazelisk compatibility, drift-checked against each other).

**`.bazelrc`** (169 lines, `/home/mherwig/dev/ocx/.bazelrc`): `startup --host_jvm_args=-Xmx2g`;
`build --jobs=12` (matches the dev host's `cargo` cap, overridden to `auto` under
`build:ci`); `build --disk_cache=~/.cache/ocx/bazel-disk` (never `/tmp`, never
workspace-relative — measured that `%workspace%` substitution only fires in
`import`/`try-import`, so a relative `--disk_cache` silently forks a second cache when
Bazel is invoked from a subdirectory); `build --remote_cache=https://bazel-cache.ocx.sh/v1`,
read-only everywhere (`--remote_upload_local_results=false`), `--remote_timeout=30s`,
`--remote_retries=2`; a **latent** `build:ci` block (`--remote_download_minimal`,
`--remote_download_regex=.*/test\.(xml|log)$` to keep JUnit/log output downloadable under
`minimal`, `--jobs=auto`) that **no lane currently activates** (`--config=ci` is parsed
nowhere in `verify-basic.yml`/`verify-deep.yml`/any taskfile recipe as of this pin).
Explicitly no credential of any kind and no `--lockfile_mode=error` in the committed rc
(scoped instead to `task bazel:mod:check`, see §C). Ends with
`try-import %workspace%/.bazelrc.user` as the **last** non-comment line — import position
is the precedence contract for personal overrides (BZL-FLAG-19).

**`.bazelignore`**: workspace-relative directory paths (not globs — a bare `target` line
does not depth-match `.gitignore`-style). Notably: `external/docker_credential/target`,
`external/rust-oci-client/target`, `external/sigstore-rs/target` are each spelled out
individually, with the comment (`.bazelignore:14-18`) that **`external` is Bazel's
reserved workspace-root directory name and `//...` never expands into it at all** — "a
package planted at `external/probe/` with nothing ignoring it stays invisible to
`bazel query 'buildfiles(//...)'`, while the same package one directory over is found."
This is corroborated externally by [bazelbuild/bazel#4508](https://github.com/bazelbuild/bazel/issues/4508)
("One cannot have 'external' top-level directory and more") — `external/` is a name Bazel
treats specially at the execroot level. Practical reading for the mirror: a BUILD package
at `external/ocx/crates/ocx_oci` is **buildable by explicit label**
(`bazel build //external/ocx/crates/ocx_oci:ocx_oci`) but **invisible to `//...`
wildcards** — any verify gate that walks `//...` must name `external/ocx/...` explicitly
or it silently skips those targets. ocx works around this only by *ignoring* the three
generated `external/*/BUILD.bazel` files from lint (root `BUILD.bazel`, see below), not by
including them in `//...` — nothing in ocx's own tree contradicts the reserved-name finding.

**Root `BUILD.bazel`**: only content is two `buildifier` targets (`buildifier.check`,
`buildifier.fix`) with an `_NOT_OURS` exclude list that includes `"./external/*/BUILD.bazel"`
verbatim, commented: "crate_universe output, written into each patched submodule by every
repin and tracked by nobody. Not ours to format and not ours to lint." Plus one
`exports_files(["Cargo.lock"], visibility = ["//crates:__subpackages__"])` — the only
root-package export.

**`rules_python`**: not a `bazel_dep` anywhere in `MODULE.bazel`. ocx's pytest acceptance
suite is **not** run through `rules_python`; it is wrapped in `sh_test` invoking `uv` via
the `rules_ocx`-provided `@tools//:uv` (see §D). **`rules_shell`** is used for `sh_test`
(`test/bazel.bzl:146`: `load("@rules_shell//shell:sh_test.bzl", "sh_test")` — noted as
non-native on a Bazel 9 pin, BZL-LARK-10) and for genrules in
`test/doc_scripts/{cast,gif}.bzl` and `website/site.bzl`.

---

### B. Commit `3d445fb6` and the generated-submodule-BUILD-file mechanism; four related commits

**`3d445fb6e01314c94c17365c2eb3cec73b94bdc1`** — "build(bazel): stop the generated
submodule BUILD files showing as untracked". **The mechanism, stated in the commit body
and DX-24** (`.claude/artifacts/plan_bazel_build_adoption.md:868`): `crate_universe`
splices each `[patch.crates-io]` path submodule (`external/rust-oci-client`,
`external/docker_credential`, `external/sigstore-rs`) during `bazel fetch`/`bazel build`
and **writes a generated `BUILD.bazel` file directly into that submodule's own working
tree** — not into a Bazel external-repo cache, the actual checked-out directory on disk.
This is not committed (CI regenerates it every run; a committed copy would be a second
source of truth) and cannot be `.gitignore`d from the superproject (a root `.gitignore`
entry cannot reach into a submodule's own `git status`, and the submodule's own
`.gitignore` belongs to the upstream fork). The fix: `task bazel:bootstrap`
(`taskfiles/bazel.taskfile.yml`) appends `BUILD.bazel` to each submodule's
`$GIT_DIR/modules/<path>/info/exclude` — the one ignore file a superproject may write
without touching the fork's tree — idempotently via `grep -qxF`, driven off `.gitmodules`
rather than a hardcoded name list, and re-run **per worktree** because
`git rev-parse --git-path` resolves to `.git/worktrees/<name>/modules/…` on this
multi-worktree machine. Diff: `.claude/artifacts/plan_bazel_build_adoption.md` (+1),
`taskfiles/bazel.taskfile.yml` (+38), `website/src/docs/contributing/bazel.md` (+2).
**Directly transfers to the mirror**: the mirror's `[patch.crates-io]` table
(`Cargo.toml:174-175`) points into `external/ocx/external/{rust-oci-client,docker_credential,sigstore-rs}`
— the identical nested-submodule shape — plus (per §F) the eight `ocx_*` path deps into
`external/ocx/crates/*` would need the same `info/exclude` treatment at the `external/ocx`
submodule level too.

**`8a8546f81445cdd5e295062337c7a18aeaad84d4`** — per-test JUnit from `test.log`. The
WP-30 lane swap (next commit) deleted the `target/nextest/ci/junit.xml` consumers
(PR annotator, otel exporter). Bazel's own synthesized `bazel-testlogs/**/test.xml`
cannot substitute: it emits one `<testcase>` per **target**, not per Rust `#[test]` fn —
a failing test's name shows up only inside testsuite-level `system-out` CDATA, and the
`<error message>` just reads `exited with error code 101`. Fix: read `test.log` (a
`testActionOutput` of the same `TestResult`, replayed on a cache hit same as `test.xml`)
via `scripts/bazel_test_floor.py --junit`, which converts libtest's own per-case log
lines into a real per-case JUnit at `target/bazel/junit.xml`.

**`02ff88f01218a22528b3f88da1ed5a8aa0214476`** — CI lane swap. `Smoke (Linux)`'s
`cargo nextest list --workspace` (536.5 s, needed only to floor the run) plus
`cargo nextest run` (173.5 s) is replaced by one `bazel test //crates/...`, whose build
event stream supplies both the run and the floor (`scripts/bazel_test_floor.py`
sums real per-case counts off `test.log`, so a shrunk target still reds even at exit 0).
macOS/Windows keep `nextest` (Linux-measured case-count floors, `cfg`-gated tests differ
per platform). One credential per lane: `main` push carries the write credential alone,
every other trigger the read credential alone.

**`2a8c663d09ef7c9bde245ed5866ecb729180d253`** — lld. `rules_cc`'s
`unix_cc_configure.bzl` autoprobes `lld` then `gold`; the GitHub runner image has
`ld.gold` but no unversioned `ld.lld`, so every rustc link action took gold and printed
rustc's own "the gold linker is deprecated and has known bugs with Rust" warning (a
miscompile risk, not cosmetic). No flag unpicks it after the fact
(`--linkopt=-fuse-ld=bfd` would leave `supports_start_end_lib` on, and GNU ld 2.45 has no
`--start-lib`); the only lever was installing `lld` on the runner image so `rules_cc`'s
own preference is satisfied.

**`18dca45194425e5e23db8bf637744934354935cd`** — acceptance suite as cached Bazel
targets. The 181 `sh_test` targets existed since WP-36 but nothing ran them —
`task verify` phase 2 and `verify-deep.yml`'s acceptance job both called
`task test:parallel` (pytest directly), bypassing Bazel entirely. Both callers now run
`task bazel:test:accept` (`bazel test //test:all --local_test_jobs=1`). Results are
**CACHED**, reversing the ADR's original stage-4 no-cache ruling (amendment measured in
`adr_bazel_build_adoption.md` § Stage 4 / DX-97): the `external` tag was removed (the
only tag that suppresses result reuse) and `local` was removed too (its `no-remote` half
suppresses the `--disk_cache` hit specifically on a *warm fresh server* — exactly the
state every CI runner is in). `no-sandbox` is kept (needed: the suite drives
`docker compose`, a `uv` venv and a built binary, none of which survive a sandboxed
working directory) and caches identically to an untagged target in both warm states. The
compensating control is `//test:suite_anchor` naming `bin/ocx*` as a declared input, so a
rebuilt binary re-keys and re-runs all 181 targets.

---

### C. Hand-written BUILD for a typical crate (`crates/ocx_oci/BUILD.bazel`)

```
load("@crates//:defs.bzl", "aliases", "all_crate_deps")
load("@rules_rust//rust:defs.bzl", "rust_library", "rust_test")

rust_library(
    name = "ocx_oci",
    srcs = glob(["src/**/*.rs"]),
    aliases = aliases(),
    compile_data = glob(["src/**"], allow_empty = True, exclude = ["src/**/*.rs"]),
    crate_features = ["__testing"],
    edition = "2024",
    proc_macro_deps = all_crate_deps(proc_macro = True),
    rustc_env = {"CARGO_MANIFEST_DIR": "crates/ocx_oci"},
    version = "0.6.2",
    deps = all_crate_deps(normal = True) + ["//crates/ocx_console", "//crates/ocx_exit", "//crates/ocx_util"],
)
```

**No shared `.bzl` macro exists for `rust_library`/`rust_test` declarations.** All 17
`crates/*/BUILD.bazel` files are hand-written per-crate, each loading `aliases` and
`all_crate_deps` straight from the `crate_universe`-generated `@crates//:defs.bzl` (a
`crate_universe` deliverable, not an ocx macro) — confirmed by `find crates -iname "*.bzl"`
returning zero results. This is the mechanism that motivated the (rejected-as-immature)
`gazelle_rust` generator evaluation in §E: hand-written is "plan ruling P3", the default
until a generator matures.

Pattern per crate: `deps = all_crate_deps(normal = True) + [explicit intra-workspace
"//crates/..." labels]` — third-party deps come from the hub macro, first-party
(intra-workspace) deps are spelled out by hand. Test targets mirror this with
`all_crate_deps(normal_dev = True, proc_macro_dev = True)` plus `"//crates/ocx_test_support"`.
`compile_data`/`data` both `glob(["src/**"], exclude = ["src/**/*.rs"])` to carry
`include_str!`/`include_bytes!` fixtures that live inside the crate directory; fixtures
that live **outside** it (root `test/` fixtures 25 files) are named explicitly via
`exports_files` in the root `BUILD.bazel` and `test/BUILD.bazel` (§A, §D) rather than
globbed, because Bazel 9 exports no source file across a package boundary implicitly and
a glob would either over- or under-declare (root `BUILD.bazel` comment, lines ~60-85).
`rust_test` targets carry heavy `args = ["--test-threads=1", "--exact", "--skip=…"]` lists
— libtest (which Bazel drives) runs testcases as threads in one process, so any test that
mutates `std::env::set_var` must be excluded per-name (still executed by `cargo nextest`,
counted separately in `crates/TEST_TARGET_MAP.toml`, which only ever rises).

`crates/ocx_cli/BUILD.bazel` is the binary-crate variant: same `rust_library` shape
(`crate_name = "ocx"`) plus a `filegroup(name = "api_data", srcs = glob(["src/api/data/**"]))`
and four separate `rust_test` targets (`ocx_cli_test`, `help_surface`,
`macos_self_contained`, `ocx_cli_bin_test` — the last with `crate_root = "src/main.rs"`,
`crate_name = "ocx_bin"`).

---

### D. pytest acceptance suite under Bazel

**Mechanism**: `test/bazel.bzl` defines `acceptance_suite(name, modules, uv = "@tools//:uv",
extra_pytest_args = None, extra_data = None)` (def at `test/bazel.bzl:406`) — one
`sh_test` per `tests/test_*.py` module plus a shared runner script
(`_acceptance_runner`). `test/BUILD.bazel` calls it once:
`load("//test:bazel.bzl", "acceptance_suite")` over `glob(["tests/test_*.py"])`, refusing
to declare zero targets if the glob matches nothing (`test/bazel.bzl:426-427`). `uv` comes
from `@tools//:uv`, resolved via the `rules_ocx` toolchain extension (§A) — not `PATH`,
not `rules_python`.

**Tags**: `ACCEPTANCE_TAGS = ["exclusive", "no-sandbox"]` (`test/bazel.bzl:157-163`,
amended by commit `18dca451`, §B — `external` and `local` were deliberately removed to
enable result caching). `exclusive` serializes the 181 targets against one shared Docker
Compose registry stack on fixed ports; measured (module docstring, `test/bazel.bzl:83-88`)
three `exclusive`-tagged tests ran with zero overlapping windows while untagged ones
overlapped pairwise — with no global `--local_test_jobs` cap, which would also serialize
the 34 unrelated Rust test targets. `no-sandbox` is required because the suite drives
`docker compose`, a `uv`-managed venv and a built `ocx` binary, none of which survive a
sandboxed execroot.

**env_inherit**: `_INHERITED_ENV` (`test/bazel.bzl:~195-220`) = `CI`, `DOCKER_CONFIG`,
`DOCKER_HOST`, `HOME`, `OCX_TEST_MIRROR_PORT`, `OCX_TEST_REGISTRY_PORT`, `SSH_AUTH_SOCK`,
`USER`, `XDG_RUNTIME_DIR`, `__OCX_TESTING_REQUIRE_ENVIRONMENT_D` — each named individually
because inherited env values are **not** part of the Bazel action cache key (documented,
named risk: a result cached with one `OCX_TEST_REGISTRY_PORT` is served for another; the
`__OCX_TESTING_REQUIRE_ENVIRONMENT_D` case is flagged as the sharp one because its two
values demand *opposite* verdicts from the same test).

**Data groups**: each target gets the module itself, the `uv` launcher, and a shared
`:suite_inputs` filegroup (conftest, pyproject/uv.lock, `ocx.toml`/`ocx.lock`, taskfile,
fixtures under `tests/**`/`scenarios/**`/`specs/**`/`sigstore/**`, `docker-compose.yml`,
`bin/ocx*` via `:suite_anchor`). Five modules that sweep sibling test source get
per-target `extra_data` rather than widening `:suite_inputs` globally (would re-run all
181 on any module edit). Documented residual under-declaration (not fixed, named
instead): reads crossing `target/**`, `website/**`, `crates/**`, `.github/**`, and
`test/doc_scripts/**` (an actual subpackage, unreachable from any glob in `test/BUILD.bazel`)
are **not** declared inputs — stale-green risk accepted and bounded because
`--remote_upload_local_results=false` means these results never leave the host that
produced them.

**Taskfile entry points** (`taskfiles/bazel.taskfile.yml`, `/usr/bin/grep -n
"^  [a-zA-Z0-9_:-]*:$"`): `bep:mode` (47), `bootstrap` (69, the submodule
`info/exclude` writer from §B), `doctor` (147, host precondition check), `pin:check`
(174), `build:nobuild` (184), `build:drift` (297), `tag:guard` (311, enforces
`ACCEPTANCE_TAGS`/`suite_anchor`/`suite_inputs` from the graph side on every `task verify`
and PR), `lint` (336), `mod:check` (360, the one place `--lockfile_mode=error` appears —
scoped here rather than the committed `.bazelrc`, §A), `test:unit` (381, `bazel test
//crates/...` plus the `bazel_test_floor.py` per-case floor and JUnit conversion, §B),
`test:accept` (517, `bazel test //test:all --local_test_jobs=1`; explicitly `//test:all`
not `//test/...` because the latter drags in 79 doc-script genrules/GIF renders — a
different lane's cost), `test` (750), `test:scoped` (786).

---

### E. Numbers — the R2 bar, the measurement, the verdict; test-speed-tier insights

**`adr_bazel_build_adoption.md` ruling R2** (quoted in `measurement_bazel_r2.md`):

> a warm-cache `bazel test //crates/...` on a GitHub-hosted runner must beat
> `cargo nextest run --workspace --profile ci` on the same commit by **≥ 4 minutes
> (240 s) median over 5 runs**, with per-lookup RTT **< 150 ms p50**. Otherwise the lane
> swap does not land.

**Metric**: wall-clock delta between the two test-execution engines, median of 5 runs,
plus an RTT gate on the remote-cache leg. **Job**: `Smoke (Linux)`'s `Test` step in
`verify-basic.yml` (the one step the swap replaces; every other step in that job is
unaffected). **Measured** (`measurement_bazel_r2.md` §1, §1c):

| State | Value |
|---|---:|
| `Smoke (Linux)` `Test` step, median over 10 recent `verify-basic` runs (the CI-scope number the verdict rests on) | **173.5 s** |
| `cargo nextest run --workspace --release --locked --profile ci`, local, warm, median of 5 | **71.01 s** |
| `bazel test //crates/...`, local, **cold** (`bazel clean` + fresh disk cache) | **174.18 s** |
| `bazel test //crates/...`, local, **warm, same server** (repeat run, server stays up) | **0.26 s** |
| `bazel test //crates/...`, local, **warm, fresh server** (`bazel clean` + `bazel shutdown` each time, disk cache warm — the CI-shaped state) | **4.44 s** |
| Local delta (cargo 71.01 s − Bazel warm-fresh-server 4.44 s) | **66.57 s** |
| R2 threshold | **≥ 240 s** |
| Shortfall | **173.43 s** (CI-scope reading) / **173.43 s** local reading is 66.57 s vs 240 s |

**RTT**: not measurable on this run — the remote-cache reader realm was implemented but
not deployed to the live host at measurement time (`C-029`), so every `--remote_cache`
lookup was a 401; no number is reported rather than substituting a wrong-scope local
figure.

**Verdict (as measured, `measurement_bazel_r2.md` top): NO-GO.** "A Bazel step costing
literally zero misses the ratified margin by 66.5 s… on this surface it is unreachable,
because the quantity being optimised is smaller than the margin demanded." `verify-deep`
contributes nothing to the case either way: it is a three-OS matrix whose wall-clock is
`max(windows, macos, linux)`, and Bazel only ever touches the Linux leg, so Stage 2's
structural contribution to that workflow's median is zero regardless of the swap.

**Important downstream fact for the mirror**: despite this measured NO-GO,
`decision_bazel_adoption.md`'s top-level verdict was **go** (owner-directed, "the decision
is the owner's scope call and not a procedure output" — Reading 4 of `go-no-go.md`), and
the git history shows the lane swap **did** eventually land (commit `02ff88f0`, §B) after
a later re-spec of R2 (`fc676337 docs(plan): settle R2 at a 647 s median…`, dated within
the same measurement window) — i.e., the bar itself was renegotiated rather than the
swap being abandoned. Read the 240 s bar in `measurement_bazel_r2.md` as the
**originally-ratified** threshold that this specific measurement failed, not as ocx's
final, standing policy.

**`adr_test_speed_tiers.md` — the pre-declared insights list** (§ "Key insights driving
this decision", lines ~57-63; this ADR is about tiered test-verification speed, not the
Bazel lane swap itself, and carries no section literally titled "Lessons"):

1. The provenance split (build-time-stamped values kept out of dev/test cache keys,
   stamped only for release) is the one pattern every researched source agrees on.
2. Cargo re-spawns itself per test because in-process re-entry of env/cwd/registry state
   is unsafe; a `rust_test` that just spawns the built binary gains no crate-level cache
   precision over that.
3. A `rust_test` placed in the top crate (`ocx_cli`, which links all 16 tier crates)
   gains speed but not cache precision — any Rust change re-runs it. Precision only comes
   from tests living in the lowest crate that actually owns the code.
4. Command-to-test mapping is cheap but trusted nowhere alone — it needs a full run on
   the protected path as a backstop.
5. No source measures escape rates for agent-specific narrow test gates; instrument it
   rather than assume it.

---

### F. Cross-module shape for the mirror — decided with evidence

**Structural precondition, checked directly**: the mirror's `external/ocx` submodule is
currently pinned at `191b9324` (2026-09-21 22:52:34), and ocx's `MODULE.bazel` was first
committed at `63c1b71f` (2026-09-22 17:57:52) — **six hours after** the mirror's current
pin. `ls external/ocx/MODULE.bazel` in the mirror confirms: not present. **Option 2 is not
available at the current pin at all**; it requires bumping the submodule past `63c1b71f`
first, and even then is disfavored for the reasons below.

**Option 1 — one crate_universe hub off the mirror's own `Cargo.toml`/`Cargo.lock`.**

The mirror's `Cargo.toml` (`/home/mherwig/dev/ocx-mirror/Cargo.toml`) already has the
exact shape ocx's own `[patch.crates-io]` path crates have:
- `[workspace] exclude = ["external/ocx"]` (Cargo.toml:16) — the eight `ocx_*` crates
  (`ocx_config`, `ocx_console`, `ocx_exit`, `ocx_index`, `ocx_oci`, `ocx_package`,
  `ocx_sign`, `ocx_util`; Cargo.toml:57-64) are `[dependencies]` **path deps to crates
  explicitly excluded from the reading workspace** — not `[workspace.members]`.
- `[patch.crates-io]` (Cargo.toml:174-175 onward) repoints `oci-client` and (per the file
  header comment, `docker_credential`/`sigstore`) into the **nested** submodules
  `external/ocx/external/{rust-oci-client,docker_credential,sigstore-rs}` — the identical
  nested-submodule, patch-table, excluded-from-workspace shape ocx itself carries for
  those same three crates one level up.

ocx's own precedent (§B, commit `3d445fb6`/DX-24) is direct evidence for what
`crate.from_cargo` does with exactly this shape: it does **not** require a pre-existing
`BUILD.bazel` for a local-path crate that is not a workspace member of the manifest being
read. Its splicer generates one and writes it **into that crate's own working-tree
directory** on every fetch/repin — observed for `external/rust-oci-client`,
`external/docker_credential`, `external/sigstore-rs` (all three: excluded from ocx's
workspace, referenced from ocx's own Cargo.lock, non-members). The mirror's eight
`ocx_*` deps and the mirror's own `[patch.crates-io]` entries are the same category of
object from `cargo metadata`'s (and therefore `crate_universe`'s) point of view: local-path
packages present in the resolved dependency graph that are not declared workspace
members of the manifest passed via `manifests =`. **Conclusion: a single
`crate.from_cargo` hub reading `//:Cargo.toml`/`//:Cargo.lock` at the mirror root should
auto-generate BUILD files for `external/ocx/crates/*` the same way it already does for
`external/ocx/external/{rust-oci-client,docker_credential,sigstore-rs}` today inside
ocx** — no hand-authored, no-commit-possible (`external/ocx/crates/*` is a
mode-160000 submodule boundary) BUILD tree is required, and DX-24's git-hygiene fix
transfers directly: `task bazel:bootstrap`'s `$GIT_DIR/modules/<path>/info/exclude`
pattern needs an entry for `external/ocx` itself, in addition to the two nested-submodule
entries the mirror will also need for its own patch table. **This is the strongest,
directly-sourced argument for Option 1**, but it is an inference from a structurally
identical precedent, not a build the mirror has actually run (builds were out of scope
here per the RAM constraint) — the first real implementation step should be a `bazel
query`/`bazel fetch --repo=@crates` smoke check to confirm the generated files land where
predicted before relying on it further.

Caveat inherited from crate_universe's own known bug
([rules_rust#3732](https://github.com/bazelbuild/rules_rust/issues/3732), open):
`crates_vendor` mode (not the bzlmod `from_cargo` hub mode ocx and the mirror both use)
has a confirmed bug where the generated **alias** for a patched crate points at the
default vendored location instead of the patched path, even though "the `rust_library`
build target is correctly generated at the patched location" — i.e. even upstream
confirms generation-at-patched-location is the intended, working behavior; only the
alias-rendering path (vendor mode only) is broken. `from_cargo` is the unaffected mode.

**Option 2 — `bazel_dep(name="ocx")` + `local_path_override(path="external/ocx")`.**
Evaluated and rejected on four independent, sourced grounds:

1. **Two hubs, duplicate third-party crates.** Mainline `rules_rust` 0.74.0 (ocx's pin)
   has no mechanism to coalesce two separate `crate.from_cargo` hubs' copies of the same
   crate into one linked library. That capability exists only in the unrelated,
   experimental fork `hermeticbuild/rules_rs`
   ([PR #255](https://github.com/hermeticbuild/rules_rs/pull/255), "coalesce crates into
   one linked lib where possible across hubs" — landing there precisely *because*
   mainline lacks it). Under Option 2 the mirror's own hub and ocx's hub would each
   resolve and build their own copy of every crate both projects share (`reqwest`, `oci-client`
   deps, `rustls`, etc. per the mirror's own `CLAUDE.md` dependency-model section) —
   two incompatible types for the same crate name, the classic Bazel Rust cross-hub
   linking failure.
2. **A non-root module's `crate.from_cargo` cannot be consumed by the root at all.**
   Per rules_rust's own non-root-module lockfile check
   ([issue #1738](https://github.com/bazelbuild/rules_rust/issues/1738) and the
   dev_dependency requirement it produced), a non-root module's `crate.from_cargo` usage
   must be marked `dev_dependency = True` or the build fails outright (a transitive
   module's lockfile cannot be repinned by the consumer). ocx's own `MODULE.bazel:52-58`
   call carries **no** `dev_dependency = True`. Marking it so would be required for ocx
   to remain valid as a non-root module — but a `dev_dependency` extension usage is,
   by bzlmod design, **not evaluated for consumers**, meaning the mirror as root could
   not `use_repo` ocx's `@crates` hub even if ocx added the flag. Either way, "consuming
   ocx's own targets" under Option 2 cannot reach ocx's third-party crate resolution.
3. **`git_override` is root-only.** Per Bazel's own module docs: "Only the root module's
   overrides take effect — if a module is used as a dependency, its overrides are
   ignored." ocx's `git_override(module_name = "rules_ocx", …)` (`MODULE.bazel:98-103`,
   itself already flagged as deliberate and temporary) would go **silently inert** the
   moment ocx is not the root module, and `rules_ocx` would fall back to the BCR registry
   — which, per ocx's own comment, "has no BCR release carrying its Bazel 9 support yet".
   Option 2 would need the mirror to re-declare that override itself, duplicating and
   permanently coupling the mirror's `MODULE.bazel` to ocx's toolchain-pin internals.
4. **`register_toolchains` is root-only too.** Per Bazel's own docs: toolchains
   registered by a non-root module "will not be registered if the current module is not
   the root module." ocx's `register_toolchains("@rust_toolchains//:all")`
   (`MODULE.bazel:44`) would likewise go inert under Option 2; the mirror would need its
   own `rust.toolchain()` call anyway, which — combined with point 1 — means the mirror
   ends up needing its own crate/toolchain resolution regardless, undermining Option 2's
   premise of "reusing ocx's own targets".
5. **Path deps outside the module root**: not actually a concern for either option —
   `external/ocx` is *inside* the mirror's own repository/module root (a submodule
   checkout under it), never outside it, so this is confirmed a non-issue by inspection
   rather than by an external source.

**`external/` reserved-name interaction with either option**: independent of Option 1 vs
2, any BUILD package placed under `external/ocx/...` — whether mirror-hand-written or
crate_universe-generated — is invisible to `//...` wildcard target patterns (§A,
`.bazelignore` comment + [bazelbuild/bazel#4508](https://github.com/bazelbuild/bazel/issues/4508)).
Any `task verify`/CI gate the mirror writes over `//...` must additionally and explicitly
name `//external/ocx/...` (or the generated crates' specific labels) or those targets are
silently never built or tested — exactly the DX-24-adjacent class of "cached stale green"
risk ocx's own `bazel_tag_guard.py` exists to catch elsewhere in that repo.

---

## Sources

**ocx repository (read-only, `/home/mherwig/dev/ocx`), via `git show|log|diff|ls-files|cat-file` only:**
- `MODULE.bazel` (full file, 119 lines)
- `.bazelrc` (full file, 169 lines), `.bazelversion`, `.bazelignore`, root `BUILD.bazel`
- `crates/ocx_oci/BUILD.bazel`, `crates/ocx_cli/BUILD.bazel`
- `test/bazel.bzl` (module docstring + `acceptance_suite()` def, line 406),
  `test/BUILD.bazel`
- `taskfiles/bazel.taskfile.yml` (task index and `mod:check`/`test:unit`/`test:accept`
  bodies)
- Commits: `3d445fb6e01314c94c17365c2eb3cec73b94bdc1`, `8a8546f81445cdd5e295062337c7a18aeaad84d4`,
  `02ff88f01218a22528b3f88da1ed5a8aa0214476`, `2a8c663d09ef7c9bde245ed5866ecb729180d253`,
  `18dca45194425e5e23db8bf637744934354935cd`; `git log -1 -- MODULE.bazel` (`63c1b71f`)
- `.claude/artifacts/plan_bazel_build_adoption.md` (DX-24, DX-25, DX-26, DX-96 rows)
- `.claude/artifacts/decision_bazel_adoption.md` (full file)
- `.claude/artifacts/measurement_bazel_r2.md` (§0, §1, §1a-c, §2)
- `.claude/artifacts/adr_test_speed_tiers.md` (Context, Key insights list, Consequences)
- `git submodule status` / `git log` cross-check against
  `/home/mherwig/dev/ocx-mirror/external/ocx` pin `191b93242b35fff0cad5c213fd4950c5b5104647`

**Mirror repository (`/home/mherwig/dev/ocx-mirror`):**
- `Cargo.toml` (workspace exclude at line 16, path deps lines 57-69, patch table from
  line 174)

**External:**
- [rules_rust `crate_universe` under bzlmod](https://bazelbuild.github.io/rules_rust/crate_universe_bzlmod.html)
- [rules_rust#3732 — patched-crate alias points at the wrong (vendored) location in `crates_vendor` mode](https://github.com/bazelbuild/rules_rust/issues/3732)
- [rules_rust#1738 / discussion #2879 — non-root module lockfile handling, `dev_dependency` requirement](https://github.com/bazelbuild/rules_rust/issues/1738)
- [hermeticbuild/rules_rs PR #255 — crate coalescence across hubs (absent from mainline rules_rust)](https://github.com/hermeticbuild/rules_rs/pull/255)
- [Bazel docs — module overrides are root-module-only](https://bazel.build/external/module)
- [Bazel docs — `register_toolchains`/`register_execution_platforms` root-module-only](https://bazel.build/rules/lib/globals/module)
- [bazelbuild/bazel#4508 — `external` is a reserved top-level directory name](https://github.com/bazelbuild/bazel/issues/4508)
