# Decision — the `ocx` (CLI crate) dependency edge in ocx-mirror

## Question

`ocx-mirror`'s `src/main.rs` names the package `ocx` (`external/ocx/crates/ocx_cli`)
for two imports — `ocx::exit::classify_library_error` and
`ocx::tracing_init::{LogLevel, LogSettings}`. Keep the edge, replace it with
mirror-owned code, or fix it upstream first?

## Research

### 1. What the three sites used before, and whether the new spellings are the true successors

`/usr/sbin/git show origin/main:src/main.rs` vs. HEAD, three differences:

| origin/main | HEAD | verdict |
|---|---|---|
| `ocx_lib::cli::{LogLevel, LogSettings}` | `ocx::tracing_init::{LogLevel, LogSettings}` | same types, relocated |
| `error.classify().unwrap_or(ocx_lib::cli::ExitCode::ConfigError)` (trait `ClassifyExitCode`) | `classify_library_error(&error)` | **wider than the site needs** — see §2 |
| `ocx_lib::log::error!` | `log::error!` | already independent (mirror has a direct `log` row) |

The `ColorMode`/`DataInterface`/`Printer`/`ProgressMode`/`clap_styles`/`ProgressManager`
group moved cleanly to `ocx_console`, which the mirror already depends on
directly. Those are not in question.

### 2. What `install_extra_roots()` can return, and whether a narrower classification reproduces the codes exactly

`src/http.rs:72` — `pub fn install_extra_roots() -> Result<(), TlsError>`.
**Exactly one concrete error type**: `ocx_config::tls::TlsError`.

`impl ClassifyExitCode for TlsError` lives at
`external/ocx/crates/ocx_cli/src/exit/ocx_config.rs`, and it is **total** —
every arm reaches `Some(code)`, there is no `None` path:

```
Unreadable{..}                                  -> IoError      (74)
TooLarge{origin,..} if origin.is_file()         -> IoError      (74)
TooLarge{..}                                    -> ConfigError  (78)
NotACertificate|Empty|Malformed|Truncated{..}   -> origin.is_file() ? DataError (65) : ConfigError (78)
```

Consequences, both established rather than assumed:

- origin/main's `.unwrap_or(ExitCode::ConfigError)` fallback was **unreachable**.
- `classify_library_error`'s `ExitCode::Failure` fallback is **equally
  unreachable** here: it walks `err -> source()`, the first cause *is* the
  `TlsError`, `TypeId` equality is exact, and `ocx_config::try_downcast` lists
  `TlsError`. So HEAD's spelling and origin/main's spelling produce the same
  code on every reachable input. The retarget is behaviour-preserving as
  written — and the whole cross-crate ladder (14 submodules, ~340 KB of
  classification, `exit.rs` 2221 lines) is dead weight for this one call.
- `TlsError` is **not** `#[non_exhaustive]`
  (`crates/ocx_config/src/tls.rs:145`), and `ExtraRootsSource::is_file()` is
  `pub` (`tls.rs:65`). An exhaustive 7-arm match in the mirror therefore
  reproduces all four reachable codes exactly **and fails to compile** if
  upstream adds a variant. It does not guard an upstream change to an existing
  arm's code — that residual risk is named in Consequences.
- The mirror **already owns this pattern**: `MirrorError::kind_exit_code()`
  (`src/error.rs:126`) is a 21-arm classifier over `ocx_exit::ExitCode`, and
  `ocx_exit` is already a direct dependency row. Classifying one more error type
  at `main` is the established shape here, not a new invention.

### 3. What `LogSettings` actually contracts, and what a copy must not get wrong

`log_settings.rs:138 build_env_filter` carries real, silently-breakable semantics:

- cascade `OCX_LOG_CONSOLE` → `OCX_LOG` → `RUST_LOG` → default `INFO`;
- **asymmetric predicate**: `OCX_LOG_*` and `OCX_LOG` are honoured on `var().is_ok()`
  (merely being set, even empty), `RUST_LOG` only when **non-empty**;
- `console_level = Some(_)` **overrides the env var entirely**
  (`builder.parse(console_level.to_string())`) — which is exactly why main.rs
  passes `None` rather than `Some(Info)`, as its own comment records;
- `stderr_color` defaults to `ColorMode::Auto.config().stderr`;
- `LogLevel`'s clap value-enum spellings are `trace|debug|info|warn|error|off`,
  and `--log-level` is the mirror's own CLI surface.

A hand-rewrite would plausibly get the `RUST_LOG`-emptiness rule and the
override-vs-default rule wrong. A **verbatim copy** does not.

Scope of an honest copy: the mirror calls only `with_console_level`,
`with_stderr_color`, `init_with_progress`. `with_filter`,
`with_console_filter`, `with_console_events`, `console_events()`, `init()` and
`build_env_filter`'s `extra_name`/`extra_filter` parameters are ocx-only. So
the copy is **~110–120 lines**, not ~250: `LogLevel` (53, mechanical), the
cascade + init (~50), the `MakeWriter` adapter (~10).
`ocx_console::progress::{LogWriter, LogWriterHandle}`, `ProgressManager::writer()`
and `LogWriter::handle()` are all `pub` (`progress.rs:193,345,350,357`), so the
adapter is writable outside `ocx_cli`.

### 4. What the edge costs, measured

`cargo metadata` resolve-graph BFS over normal+build edges from `ocx_mirror`,
run twice — once whole, once with the `ocx` edge cut:

| | packages |
|---|---|
| whole graph (normal + build, incl. root) | **743** |
| with the `ocx` row removed | **583** |
| **attributable solely to the `ocx` row** | **160** |
| — of which `gix`/`vergen` (build-dep) | 49 |
| — other | 111 |

The 111 include: the entire Starlark host via `ocx_script` — `starlark`,
`starlark_derive`, `starlark_map`, `starlark_syntax`, `lalrpop`, `logos`,
`lsp-types`, `rustyline`, `debugserver-types`, `annotate-snippets`,
`allocative`, `schemafy` ×3 (an LSP + debug-adapter + REPL stack, in a mirror
binary); four ocx crates the mirror never names (`ocx_project`,
`ocx_package_manager`, `ocx_script`, `ocx_setup`); and ~6 the mirror genuinely
needs (`tracing-subscriber`, `tracing-log`, `matchers`, `sharded-slab`,
`thread_local`, `nu-ansi-term`).

**Net effect of dropping the row: −160 +≈6 = −154 packages.**

The "89 added, 1 removed" lock figure understates this, because the retarget
also drops packages elsewhere; 160 is the number that answers "what does this
row cost".

`deny.toml` — three RUSTSEC ignores (`RUSTSEC-2024-0388` derivative,
`2024-0436` paste, `2025-0057` fxhash) and two license allowances (`BSL-1.0`
via `clipboard-win`→`error-code`, `BSD-2-Clause` via
`debugserver-types`→`schemafy`→`Inflector`) exist **only** for the starlark
family, and every one of their own comments says "REMOVE when … after a
starlark family bump". Starlark reaches the mirror only through the `ocx` row.
`/usr/sbin/git diff origin/main..HEAD -- deny.toml` shows one comment-only
change, so these are **not a regression this retarget introduced** — they came
via `ocx_lib` before. They are a cleanup dropping the row makes available, and
a cost keeping it locks in.

**Build time.** Cold, isolated, `-j 12`, fresh target dir: the `vergen-gix`
build-dep subgraph (49 crates) is **17.45 s wall, 283 MB peak RSS**. That is
the only compile cost genuinely *new* versus origin/main, and it is small.
I could **not** measure the starlark cluster: `starlark = "=0.13.0"` does not
compile as a standalone edition-2024 crate here (`starlark_map` errors), so the
12.41 s reading from that probe is a failed partial build and is not evidence.
I did **not** time a full cold mirror build either way.

### 5. Option C, honestly

Two separate moves, each blocked by a stated, enforced invariant:

- **`ClassifyExitCode` / `classify_library_error` → `ocx_exit`.**
  `crates/ocx_test_support/tests/workspace_structure.rs:1398
  no_classification_in_libraries` scans **every `crates/*/src` except `ocx`**
  for the needles `ClassifyExitCode` and `ClassifyErrorKind`. Moving the trait
  reds it. And the trait alone is useless to the mirror — what the mirror needs
  is the `impl … for TlsError`, i.e. the policy, which would have to follow into
  `ocx_config`, reddening the same test. `exit.rs`'s own doc: *"Classification is
  a CLI concern: only a process that exits owns the mapping from an error to an
  exit code … what `no_classification_in_libraries` enforces (C-030)."* This is
  not a relocation, it is overturning C-030 / DEC-23.
- **`tracing_init` → `ocx_console`.** Requires adding `tracing-subscriber` to
  `crates/ocx_console/Cargo.toml`, whose own comment reads *"The facade only —
  no subscriber (D-009)"*, and which `tracing_init.rs:11` names as the
  enforcement mechanism. No cycle against `crate_map.toml`
  (`ocx_console = ["ocx_exit", "ocx_util"]`; `tracing-subscriber` is not an ocx
  crate), so the blocker is the invariant, not the map. Weighed straight: that
  doc comment is right, and C as framed is wrong.

**And rule 2 does not require C.** `classify_library_error` is not the SDK-facing
ErrorCode interface. `ocx_exit` is — a standalone crate, 491 lines, **one**
runtime dependency (`serde`), already a direct row in the mirror's manifest. An
SDK that "calls ocx **as a binary**" reads an integer process status; it never
holds an in-process `ocx_config::tls::TlsError`, so it has no use for a
cross-crate downcast ladder. Rule 2 is already satisfied.

## Decision

**B — drop the `ocx` row. The mirror owns both pieces.**

One ocx-mirror PR, no upstream blocker:

1. Delete `ocx = { path = "external/ocx/crates/ocx_cli" }` from `Cargo.toml`
   (and the paragraph of its comment that explains it); add
   `tracing-subscriber` with the feature list copied exactly from ocx's
   `[workspace.dependencies]`.
2. Add `src/tracing_init.rs` — `LogLevel`, a trimmed `LogSettings`
   (`with_console_level`, `with_stderr_color`, `init_with_progress`,
   `build_env_filter`) and the `ProgressLogWriter` `MakeWriter` adapter, copied
   **verbatim** from `external/ocx/crates/ocx_cli/src/tracing_init{.rs,/*.rs}`
   with the unused builder methods dropped. ~110–120 lines.
3. Replace `classify_library_error(&error)` in `main.rs` with an exhaustive
   7-arm `match` on `TlsError`, reproducing the four reachable codes
   (74/74/78/65-or-78), sited next to the existing `MirrorError::kind_exit_code`
   convention. ~20 lines.
4. Drop the three now-unreachable `RUSTSEC-*` ignores and the `BSL-1.0` /
   `BSD-2-Clause` allowances from `deny.toml`, with `cargo deny check` green as
   the evidence. Touch the two `ocx = { path = … }` mentions in `CLAUDE.md` and
   `.serena/memories/tech_stack.md`.

Rationale, three points:

- **Rule 1 is decisive.** *"each crate is logically independent and has a close
  interface that is not explicitly designed for another crate"* — `ocx` is an
  application, not a crate with an interface. A second application linking it is
  precisely the coupling rule 1 forbids, and it costs 160 packages, an LSP/REPL
  stack and five dependency-policy exemptions for two imports.
- **Rule 2 is not in tension.** `ocx_exit` already is the reusable ErrorCode
  crate. C buys nothing for it and pays with C-030 and D-009.
- **Rule 3 holds exactly, not approximately.** §2 proves the classification is
  reproducible byte-for-byte on every reachable input, and §3's copy is verbatim.
  Nothing is cut (rule 4): the mirror keeps every behaviour it has today.

**Follow-up, upstream, not blocking.** Open an ocx issue to extract
`crates/ocx_cli/src/tracing_init{.rs,/log_level.rs,/log_settings.rs}` (~290
lines) into a leaf crate `ocx_tracing` depending on `ocx_console` +
`tracing-subscriber` + `clap_builder` + `log`. That violates **neither** C-030
(no classification in it) **nor** D-009 (`ocx_console` still names no
subscriber) — it is the version of C that survives contact with the invariants.
Cost: a crate dir + manifest, 3 files moved verbatim, one `crate_map.toml` row
(`ocx_tracing = ["ocx_console", "ocx_util", "ocx_exit"]`) plus `ocx_tracing` on
the `ocx` row, `ocx_cli` swapping `mod tracing_init` for the dep, and its ~5
call sites — roughly 8 files, ~350 lines moved, ~30 written. The mirror then
deletes its copy on the next submodule bump. The exit classification stays with
the mirror permanently; that is what C-030 says it is for.

## Consequences

- **Accepted risk, single item:** the `TlsError` arm-to-code mapping is
  duplicated. A new upstream variant is caught at compile time (the enum is not
  `#[non_exhaustive]`); a changed **code** on an existing arm is not. Mitigation
  if wanted, one line in the PR: a mirror unit test asserting the four codes by
  constructing each `TlsError` variant — cheap, and it makes the drift loud at
  the next submodule bump.
- The `tracing_init` copy is real duplication until the follow-up lands, and the
  `OCX_LOG` cascade is a contract shared with `ocx`. Verbatim copying plus the
  named follow-up is how that is paid down; an approximate rewrite is not.
- The mirror gains a direct `tracing-subscriber` row whose features must track
  ocx's, joining the feature lists the manifest already says to keep in sync.
- Build graph: −154 packages. Starlark, lalrpop, schemafy, rustyline, lsp-types
  and the gix family all leave the mirror's build entirely.
- One PR, no merge ordering, no submodule re-bump.

## What I could not establish

- **A full cold mirror build was not timed**, either with or against the row. I
  timed only the isolated `vergen-gix` subgraph (17.45 s, 12 jobs, fresh target
  dir). The 160/583/743 package counts are exact (`cargo metadata` resolve
  graph); the wall-clock consequence of 154 of them is not measured.
- **The starlark cluster's compile cost is unmeasured** — `starlark 0.13` fails
  to build as a standalone edition-2024 probe crate, so I have no honest number
  for it. The 12.41 s that probe printed is a failed build and is not cited as
  evidence anywhere above.
- **`cargo deny check licenses` was not run.** `deny.toml`'s own comments
  attribute all five exemptions to the starlark family and `cargo metadata`
  confirms starlark is `ocx`-only, so step 4 above is predicted, not verified.
  Verify it in the PR rather than on my word.
- Whether ocx maintainers would accept the `ocx_tracing` extraction. I read the
  invariants; I did not consult anyone.
