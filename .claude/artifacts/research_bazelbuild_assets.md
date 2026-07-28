# Research: bazelbuild upstream assets (buildtools + bazel)

Inventory for the four new specs that join `bazelisk` in `ocx-contrib/mirror-bazelbuild`.
Verified against the GitHub releases API 2026-07-28. Feeds WP-F of
[`plan_mirror_capability_cut.md`](./plan_mirror_capability_cut.md).

## Corrections to the bazelisk precedent

- **No `v` prefix.** `buildtools` and `bazel` tags are bare `X.Y.Z` (`releases/tags/6.0.0`
  → 200, `releases/tags/v6.0.0` → 404). Use `^(?P<version>\d+\.\d+\.\d+)$`, NOT bazelisk's
  `^v(?P<version>…)$`.
- **`platforms:` applicability is spec-level, not per-variant.** `MirrorSpec::platform_applies`
  (`src/spec.rs:281`) looks the raw platform string up in `self.platforms` — there is no
  variant axis in that key. A key like `linux/arm64/nojdk` would match nothing and silently
  do nothing. Do not write one.

## buildtools → three packages

Apache-2.0. Tags `6.0.0`, `7.1.2`, `8.2.1`, `8.5.1`.

| Platform | Asset | Present since |
|---|---|---|
| linux/amd64 | `<tool>-linux-amd64` | ≥ 5.0.0 |
| linux/arm64 | `<tool>-linux-arm64` | ≥ 5.0.0 |
| darwin/amd64 | `<tool>-darwin-amd64` | ≥ 5.0.0 |
| darwin/arm64 | `<tool>-darwin-arm64` | ≥ 5.0.0 (absent 4.0.1) |
| windows/amd64 | `<tool>-windows-amd64.exe` | ≥ 5.0.0 |
| windows/arm64 | `<tool>-windows-arm64.exe` | **8.5.1 only** |

`<tool>` ∈ {`buildifier`, `buildozer`, `unused-deps`}.

**Naming discontinuity:** the third tool is `unused_deps-*` (underscore) through 8.2.1 and
`unused-deps-*` (hyphen) from 8.5.1. One anchored pattern covers both — `^unused[_-]deps-linux-amd64$`
— and stays unambiguous because only one spelling exists per release.

Excluded by anchoring: the bare unversioned `buildifier` asset (4.0.1 legacy, no platform
suffix), and the intermittent `linux-riscv64` / `linux-s390x` builds (present 7.3.1, absent
8.0.0, back 8.2.0 — irrelevant unless the platform set widens, in which case 8.0.0 needs an
`exclude`).

`versions.min: "5.0.0"` — first release with darwin/arm64 for all three. 4.0.1 confirmed
lacking it; the range between was not bisected, so 5.0.0 is a safe floor, not proven to be
the exact origin.

## bazel → one package, two variants

Apache-2.0. Tags `6.0.0`, `7.4.0`, `9.2.0` — same bare form.

Default assets `bazel-<version>-<os>-<arch>[.exe]`; the nojdk variant is identical but
prefixed `bazel_nojdk-` (hyphen → underscore separator). Anchored per platform:

```
linux/amd64    ^bazel-.*-linux-x86_64$          ^bazel_nojdk-.*-linux-x86_64$
linux/arm64    ^bazel-.*-linux-arm64$           ^bazel_nojdk-.*-linux-arm64$
darwin/amd64   ^bazel-.*-darwin-x86_64$         ^bazel_nojdk-.*-darwin-x86_64$
darwin/arm64   ^bazel-.*-darwin-arm64$          ^bazel_nojdk-.*-darwin-arm64$
windows/amd64  ^bazel-.*-windows-x86_64\.exe$   ^bazel_nojdk-.*-windows-x86_64\.exe$
windows/arm64  ^bazel-.*-windows-arm64\.exe$    ^bazel_nojdk-.*-windows-arm64\.exe$
```

The two variants are disjoint by literal prefix. Anchoring already excludes `*.sha256`,
`*.sig`, `bazel-*-dist.zip`, `bazel-*-installer-*.sh`, `bazel_*-linux-x86_64.deb`, and the
Windows `.zip` beside the `.exe`.

Coverage gaps: default gains darwin/arm64 at 5.0.0 and windows/arm64 at 6.0.0; nojdk gains
windows at 6.0.0 and linux/arm64 at 7.0.0 (6.5.0 confirmed lacking, 7.0.0 confirmed having,
in-between not bisected).

**No `platforms:` floors are needed for those gaps.** An asset that does not exist simply
does not resolve, so the pair never enters the plan and the workflow's test job skips it
explicitly ("platform not in version's set",
`generate/templates/workflow.yml:143-157`). Floors would only restate what the resolver
already does. `versions.min: "6.0.0"` for the whole spec.

## Catalog copy

Drafted and ready to paste as each spec directory's `CATALOG.md`. Structure matches the
live bazelisk entry (frontmatter → H1 → What's included → Usage → Links). Each closes by
naming its two siblings, since all three ship from one upstream release and a reader who
found one should learn the others exist. Upstream's source directory is still literally
`unused_deps` — only the released asset took the hyphen at 8.5.1 — so the upstream link
keeps the underscore while the package name uses the hyphen.

One claim needs trimming before it ships: the drafts suggest piping `unused-deps` output
into `buildozer -f -`. Upstream documents only that it "outputs buildozer commands"; the
pipe is a plausible composition, not a documented one. State what upstream states.

<details>
<summary>buildifier</summary>

```markdown
---
title: Buildifier
description: Formatter and linter for Bazel BUILD, WORKSPACE, and .bzl files — the standard code style for Bazel's Starlark dialect
keywords: buildifier,bazel,buildtools,formatter,linter,starlark,build-file,bazelbuild
---

# Buildifier

Buildifier formats and lints Bazel `BUILD`, `WORKSPACE`, and `.bzl` files to a
standard style, the way `gofmt` does for Go — anyone maintaining a Bazel
workspace wants it in their pre-commit hook or CI so generated and
hand-written BUILD files never drift apart.

It reads Starlark source (`BUILD`/`BUILD.bazel`, `WORKSPACE`, `.bzl`,
`MODULE.bazel`) and either rewrites the file in place or reports a
diagnostic, depending on the flags passed.

## What's included

- **buildifier** — the single static binary. No runtime dependencies.

## Usage

​```sh
buildifier -r .
​```

Reformats every BUILD/WORKSPACE/.bzl file under the current directory in
place. Run with `--lint=warn` to report style issues without rewriting, or
`--mode=check --format=json` to fail a CI step non-destructively.

Buildifier ships from the same repository and release as **buildozer** and
**unused-deps** — if you need one, the other two are one `ocx add` away.

## Links

Apache-2.0 licensed.

- [buildifier on GitHub](https://github.com/bazelbuild/buildtools/tree/master/buildifier)
- [buildtools on GitHub](https://github.com/bazelbuild/buildtools)
```

</details>

<details>
<summary>buildozer</summary>

```markdown
---
title: Buildozer
description: Command-line tool for scripted, bulk edits to Bazel BUILD files — add, remove, or rename targets and attributes across a workspace
keywords: buildozer,bazel,buildtools,build-file,refactoring,bazelbuild
---

# Buildozer

Buildozer rewrites Bazel `BUILD` files from the command line using a small
set of standard commands — anyone doing a mechanical, repo-wide BUILD-graph
refactor (renaming a target, swapping a dependency, migrating a load
statement) reaches for it instead of hand-editing hundreds of files or
writing a one-off script.

It takes a command string and one or more target labels, and rewrites the
BUILD file(s) that define those targets in place.

## What's included

- **buildozer** — the single static binary. No runtime dependencies.

## Usage

​```sh
buildozer 'add deps //base' //pkg:rule //pkg:rule2
​```

Adds `//base` to the `deps` of `//pkg:rule` and `//pkg:rule2`. Commands can
also be batched from a file with `buildozer -f commands.txt`.

Buildozer ships from the same repository and release as **buildifier** and
**unused-deps** — if you need one, the other two are one `ocx add` away.

## Links

Apache-2.0 licensed.

- [buildozer on GitHub](https://github.com/bazelbuild/buildtools/tree/master/buildozer)
- [buildtools on GitHub](https://github.com/bazelbuild/buildtools)
```

</details>

<details>
<summary>unused-deps</summary>

```markdown
---
title: Unused Deps
description: Static analysis for Bazel java_library targets that finds unused dependencies and emits the buildozer commands to remove them
keywords: unused-deps,unused_deps,bazel,buildtools,java,dependencies,bazelbuild
---

# Unused Deps

`unused-deps` finds dependencies listed on a Bazel `java_library` target that
the code never actually uses — anyone trying to keep a Java BUILD graph
lean, or trim a target's compile-time closure, runs it before pruning deps
by hand.

It takes one or more target labels, checks each `java_library`'s declared
`deps` against what its sources actually import, and prints the `buildozer`
commands that would remove the unused ones. It never edits a file itself.

## What's included

- **unused-deps** — the single static binary. No runtime dependencies.

## Usage

​```sh
unused-deps //pkg:my_java_library
​```

Prints a `buildozer 'remove deps ...'` line for each dependency the target
doesn't need.

`unused-deps` ships from the same repository and release as **buildifier**
and **buildozer** — if you need one, the other two are one `ocx add` away.

## Links

Apache-2.0 licensed.

- [unused_deps on GitHub](https://github.com/bazelbuild/buildtools/tree/master/unused_deps)
- [buildtools on GitHub](https://github.com/bazelbuild/buildtools)
```

</details>

Flag when pasting: the fenced blocks above carry a zero-width joiner before their inner
backticks so they survive nesting here. Strip it.

## Spec drafts

All three binaries define a Go `-version` flag that prints and exits 0 without touching a
workspace, so `command: <tool> --version` is a valid smoke test for each — no Bazel install
needed in the test sandbox. `TestEntry` accepts `command`, `script`, or `script_inline`
(exactly one, `src/spec/tests_config.rs:42`), so the docs' "command-only" claim is wrong but
harmless here.

Canonical spec — `buildifier/mirror.yml`:

```yaml
name: buildifier
target:
  registry: ghcr.io
  repository: ocx-contrib/bazelbuild/buildifier

source:
  type: github_release
  owner: bazelbuild
  repo: buildtools
  tag_pattern: "^(?P<version>\\d+\\.\\d+\\.\\d+)$"

assets:
  linux/amd64:   ["^buildifier-linux-amd64$"]
  linux/arm64:   ["^buildifier-linux-arm64$"]
  darwin/amd64:  ["^buildifier-darwin-amd64$"]
  darwin/arm64:  ["^buildifier-darwin-arm64$"]
  windows/amd64: ["^buildifier-windows-amd64\\.exe$"]
  windows/arm64: ["^buildifier-windows-arm64\\.exe$"]

# Intentionally EXCLUDED (anchoring alone suffices, no dedicated pattern):
#   *-linux-riscv64, *-linux-s390x — real upstream platforms we do not mirror
#   bare "buildifier" with no platform suffix — pre-5.0.0 legacy, below versions.min

asset_type:
  type: binary
  name: buildifier

metadata:
  default: metadata.json

catalog:
  logo: ../logo.svg

skip_prereleases: true
cascade: true
build_timestamp: none

versions:
  min: "5.0.0"
  new_per_run: 5
  poll_interval: "0 */6 * * *"

verify:
  github_asset_digest: true

tests:
  - name: version
    command: buildifier --version

announce:
  package: bazelbuild/buildifier
  fork: ocx-contrib/index
```

`buildozer` and `unused-deps` are the same file with five substitutions: `name`,
`target.repository`, the six asset patterns' prefix, `asset_type.name`, `tests[].command`,
and `announce.package`. Stagger `poll_interval` to `20 */6 * * *` and `40 */6 * * *` so the
three do not hit the API in the same minute. `unused-deps` needs `^unused[_-]deps-…$` for
the 8.5.1 rename, with the reason in a comment.

`metadata.json`, identical for all three (single self-contained static binary — the live
bazelisk one is already tool-agnostic):

```json
{
  "type": "bundle",
  "version": 1,
  "env": [
    { "key": "PATH", "type": "path", "required": true,
      "value": "${installPath}", "visibility": "public" }
  ]
}
```

### Corrections to the draft, verified against the parser

- **`catalog:` takes `readme:` and `logo:`, not `default:`.** `CatalogConfig`
  (`src/spec/catalog_config.rs:22`) is `#[serde(deny_unknown_fields)]`, so a `default:` key
  is a hard spec-load failure (65), not a warning. `readme` already defaults to `CATALOG.md`,
  so the whole block collapses to the one line that is actually load-bearing —
  `logo: ../logo.svg`, needed because the default probe looks in the **spec** directory and
  the logo is shared at the repo root.
- **Omitting `platforms:` is correct and is not an oversight.** `platforms:` is the
  version-applicability map (`min_version`/`max_version`/`exclude`,
  `MirrorSpec::platform_applies`), not a CI runner matrix. The live bazelisk spec has no
  such block and generates CI fine.
- **Multi-spec hazard for later:** `TestEntry::script` paths are documented as relative to
  the **mirror repo root**, not the spec directory. In a multi-spec repo every `script:`
  must therefore be written `<package-dir>/tests/foo.star`. Only `command:` tests are used
  here, so nothing is broken today — but WP-A's docs should say it, or the first
  Starlark test in a subdirectory spec will fail on a path that looks right.

## Open

Whether `bazel` belongs in `mirror-bazelbuild` at all, or in its own repo — it is a far
larger and more visible package than the three buildtools binaries. Decide before authoring.
