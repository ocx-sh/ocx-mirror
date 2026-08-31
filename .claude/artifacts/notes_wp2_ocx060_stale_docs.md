# Notes — 0.5-era spellings still in prose after the ocx 0.6 CLI rename (WP2)

Every **live** call site was renamed and is covered by a discriminating test.
What follows is prose only: doc comments and one module-level `//!` block that
still name a flag or verb ocx 0.6 renamed. None of it affects behaviour; all of
it will read as wrong to the next person who greps for a spelling.

Recorded 2026-08-31 by WP2 (CLI verb/flag rename). Verified against
`ocx 0.6.0` on PATH, not against the vendored submodule.

## Stale doc comments — `--tags-from-file` → `--tags-file`

The live emitter was `src/pipeline/ocx_cli/announce.rs:73`; it now pushes
`--tags-file` and `build_announce_args_uses_additive_tags_file_never_replacing_tags`
reds if that regresses. These are the surviving prose references:

| File:line | Current text | Should say |
|---|---|---|
| `src/command/package/pipeline/announce.rs:14` | `//! \`--tags-from-file\` it can never drop a committed tag, and yank markers` | `--tags-file` |
| `src/command/package/pipeline/cascade.rs:258` | `/// \`ocx package announce --tags-from-file\` splits on newlines and would take a` | `ocx package announce --tags-file` |
| `src/command/package/pipeline/cascade.rs:326` | `/// A blank line reaching \`--tags-from-file\` is announced as a tag, so the` | `--tags-file` |
| `src/command/package/pipeline/push.rs:813` | `/// \`--tags-from-file\` is additive, and \`ocx package announce\` re-observes every tag` | `--tags-file` |
| `src/run_summary.rs:137` | `/// Tags handed to \`--tags-from-file\`, in run order.` | `--tags-file` |

Not touched because `push.rs`, `cascade.rs` and `announce.rs` were outside
WP2's path list and `run_summary.rs` was never assigned.

## Stale doc comment — `package describe` → `package description push`

| File:line | Current text | Should say |
|---|---|---|
| `src/command/package/pipeline/plan/env.rs:277` | `/// \`ocx package describe\` subprocess failures.` | `ocx package description push` |

`plan/env.rs` was explicitly excluded from WP2's scope. The live spawn at
`src/command/package/pipeline/describe.rs` **was** fixed (`["package",
"description", "push", …]`) and now has a discriminating test.

## Deliberate, not stale

Two `ocx run` occurrences are intentional and must **not** be "fixed":

- `src/command/package/pipeline/generate/ci/tests/test_entries.rs:157-158`
- `src/command/package/pipeline/generate/ci/tests/golden.rs:43`

Both are negative guards that bar a wrapper under *either* spelling
(`["ocx exec", "ocx run"]`). Dropping `run` would let a pre-0.6 wrapper back in
unnoticed. `docs/reference/environment.md:95` likewise names `--tags-from-file`
on purpose — it is documenting what the flag replaced.

## Coverage gap: the container-leg release curl is not in any golden

`releases/download/{OCX_CLI_TAG}` (`src/command/package/pipeline/generate/ci/matrix.rs:394`)
has **zero** occurrences under `tests/golden/` — by construction, not by
oversight. `golden.rs`'s `NATIVE_FIXTURES` list is native-only (specs with no
`containers:`), and the curl is emitted only for container legs.

**It is asserted, though, just not by a golden:**

- `src/command/package/pipeline/generate/ci/tests/container_legs.rs:717-723`,
  in `fn container_legs_fetch_a_libc_matched_ocx_per_architecture()` (line 701),
  renders a container spec and asserts the workflow contains
  `https://github.com/ocx-sh/ocx/releases/download/{OCX_CONTAINER_CLI_TAG}/ocx-${OCX_TRIPLE}.tar.gz`.

The limitation worth recording: that assertion interpolates the **same constant**
the renderer does, so it proves the curl tracks `OCX_CONTAINER_CLI_TAG` but can
never catch a wrong *value* of it. Only a golden or a literal would. Since the
constant also drives `version:` — which 92 golden lines now pin at `"0.6.0"` —
a wrong constant is caught there instead. So the value is covered transitively;
adding a container fixture to `NATIVE_FIXTURES` would be the direct fix, at the
cost of the deliberate friction that list exists to impose.
