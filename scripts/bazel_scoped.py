# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""`task bazel:test:scoped` — changed files vs a base → the Bazel tests they reach.

    scripts/bazel_scoped.py --base origin/main --out <targets.txt> --bazel '<bazel argv>'
    scripts/bazel_scoped.py --self-test   # no git, no bazel

C-010. Changed = `git diff --name-only --no-renames $(git merge-base BASE HEAD)`
plus untracked files. `select` decides, with the filesystem and the Bazel
query passed in:

  escalate   MODULE.bazel(.lock), .bazelversion, .bazelrc, .bazelignore,
             Cargo.lock, any Cargo.toml or BUILD.bazel, external/ocx
             → the out file reads `ESCALATE` (the task runs `bazel:test`)
  test/**    `//test:acceptance`
  otherwise  each file as a label of its owning package (a deleted file:
             that package's `:all`); a `--keep_going` resolve drops files no
             rule reads, then `kind(test, rdeps(//..., set(<labels>)))`

The out file holds the space-separated selection, or an empty line.
"""

from __future__ import annotations

import argparse
import dataclasses
import shlex
import subprocess
import sys
from collections.abc import Callable
from pathlib import Path

from _gate import expect

ESCALATE = "ESCALATE"
ESCALATE_FILES = frozenset(
    {"MODULE.bazel", "MODULE.bazel.lock", ".bazelversion", ".bazelrc", ".bazelignore", "Cargo.lock"}
)
ESCALATE_NAMES = ("BUILD.bazel", "Cargo.toml")
ACCEPTANCE = "//test:acceptance"

#: `query(expr, keep_going)` → the labels it printed; exits on a failed query.
Query = Callable[[str, bool], set[str]]


@dataclasses.dataclass(frozen=True)
class Selection:
    escalate: list[str]
    targets: list[str]
    dropped: list[str]


def lines(text: str) -> set[str]:
    return {line for line in text.splitlines() if line}


def package(path: str, has_build: Callable[[Path], bool]) -> str | None:
    for parent in Path(path).parents:
        if has_build(parent):
            return "" if str(parent) == "." else str(parent)
    return None


def select(changed: set[str], has_build: Callable[[Path], bool], exists: Callable[[str], bool], query: Query) -> Selection:
    escalate = sorted(
        f
        for f in changed
        if f in ESCALATE_FILES or Path(f).name in ESCALATE_NAMES or f == "external/ocx" or f.startswith("external/ocx/")
    )
    if escalate:
        return Selection(escalate, [], [])
    selected: set[str] = set()
    labels: list[str] = []
    for f in sorted(changed):
        if f.startswith("test/"):
            selected.add(ACCEPTANCE)
            continue
        pkg = package(f, has_build)
        if pkg is None:
            continue
        rel = Path(f).relative_to(pkg) if pkg else Path(f)
        labels.append(f"//{pkg}:{rel}" if exists(f) else f"//{pkg}:all")
    dropped: list[str] = []
    if labels:
        resolved = query("set(" + " ".join(labels) + ")", True)
        dropped = sorted(set(labels) - resolved - {label for label in labels if label.endswith(":all")})
        if resolved:
            selected |= query(f"kind(test, rdeps(//..., set({' '.join(sorted(resolved))})))", False)
    return Selection([], sorted(selected), dropped)


def bazel_query(
    bazel: list[str], expr: str, keep_going: bool, run: Callable[..., subprocess.CompletedProcess[str]] = subprocess.run
) -> set[str]:
    cmd = [*bazel, "query", *(["--keep_going"] if keep_going else []), "--output=label", expr]
    result = run(cmd, capture_output=True, text=True, encoding="utf-8", check=False)
    # --keep_going exits 3 on a partial result (the non-target paths); any
    # other non-zero is a query that did not run, never "nothing to test".
    if result.returncode not in ((0, 3) if keep_going else (0,)):
        sys.exit(f"bazel test:scoped: `{' '.join(cmd)}` exited {result.returncode}: {result.stderr[-600:]}")
    return lines(result.stdout)


def git(*args: str) -> str:
    cmd = ["git", *args]
    result = subprocess.run(cmd, capture_output=True, text=True, encoding="utf-8", check=False)
    if result.returncode != 0:
        sys.exit(f"bazel test:scoped: `{' '.join(cmd)}` exited {result.returncode}: {result.stderr[-600:]}")
    return result.stdout


def run(base: str, out: Path, bazel: list[str]) -> int:
    merge_base = git("merge-base", base, "HEAD").strip()
    changed = lines(git("diff", "--name-only", "--no-renames", merge_base))
    changed |= lines(git("ls-files", "--others", "--exclude-standard"))
    selection = select(
        changed,
        lambda parent: (parent / "BUILD.bazel").is_file(),
        lambda f: Path(f).exists(),
        lambda expr, keep_going: bazel_query(bazel, expr, keep_going),
    )
    if selection.escalate:
        out.write_text(ESCALATE + "\n", encoding="utf-8")
        print(f"bazel test:scoped: {len(changed)} changed path(s); escalating to bazel:test on {selection.escalate}")
        return 0
    if selection.dropped:
        print(
            f"bazel test:scoped: {len(selection.dropped)} path(s) are no Bazel target - dropped: {selection.dropped}",
            file=sys.stderr,
        )
    out.write_text(" ".join(selection.targets) + "\n", encoding="utf-8")
    print(
        f"bazel test:scoped: {len(changed)} changed path(s) select {len(selection.targets)} target(s): "
        f"{' '.join(selection.targets) or '(none)'}"
    )
    return 0


# ---------------------------------------------------------------------------
# Self-test — a fake tree and a fake query, no git, no bazel.
# ---------------------------------------------------------------------------

PACKAGES = {Path("."), Path("crates/ocx_mirror_http"), Path("test")}
FILES = {"src/main.rs", "crates/ocx_mirror_http/src/lib.rs", "docs/index.md"}
#: What `--keep_going` resolves: docs/ is no target, `:all` expands.
RESOLVES = {
    "//:src/main.rs": {"//:src/main.rs"},
    "//crates/ocx_mirror_http:src/lib.rs": {"//crates/ocx_mirror_http:src/lib.rs"},
    "//crates/ocx_mirror_http:all": {"//crates/ocx_mirror_http:ocx_mirror_http"},
    "//:docs/index.md": set(),
}
RDEPS = {
    "//:src/main.rs": {"//:ocx_mirror_test"},
    "//crates/ocx_mirror_http:src/lib.rs": {"//crates/ocx_mirror_http:ocx_mirror_http_test"},
    "//crates/ocx_mirror_http:ocx_mirror_http": {"//crates/ocx_mirror_http:ocx_mirror_http_test"},
}


class FakeQuery:
    def __init__(self) -> None:
        self.calls: list[tuple[str, bool]] = []

    def __call__(self, expr: str, keep_going: bool) -> set[str]:
        self.calls.append((expr, keep_going))
        inner = expr.rsplit("set(", 1)[1].split(")", 1)[0].split()
        table = RESOLVES if keep_going else RDEPS
        unknown = [label for label in inner if label not in table]
        expect(not unknown, f"queried labels the fake tree does not hold: {unknown} in {expr}")
        return set().union(*(table[label] for label in inner))


def fake_select(changed: set[str]) -> tuple[Selection, FakeQuery]:
    query = FakeQuery()
    return select(changed, lambda parent: parent in PACKAGES, lambda f: f in FILES, query), query


def fake_run(returncode: int) -> Callable[..., subprocess.CompletedProcess[str]]:
    def run(cmd: list[str], **_: object) -> subprocess.CompletedProcess[str]:
        return subprocess.CompletedProcess(cmd, returncode, stdout="//:a\n", stderr="FATAL: crashed")

    return run


def self_test() -> int:
    checks = 0
    for trigger in [
        "MODULE.bazel",
        "MODULE.bazel.lock",
        ".bazelversion",
        ".bazelrc",
        ".bazelignore",
        "Cargo.lock",
        "Cargo.toml",
        "BUILD.bazel",
        "crates/ocx_mirror_http/BUILD.bazel",
        "crates/ocx_mirror_http/Cargo.toml",
        "external/ocx",
        "external/ocx/crates/ocx_oci/src/lib.rs",
    ]:
        selection, query = fake_select({trigger, "src/main.rs"})
        expect(selection.escalate == [trigger], f"{trigger} did not escalate: {selection}")
        expect(not query.calls, f"{trigger} escalated after querying bazel: {query.calls}")
        checks += 1

    selection, query = fake_select({"test/tests/test_mirror.py", "test/src/helpers.py"})
    expect(selection == Selection([], [ACCEPTANCE], []), f"test/** did not select {ACCEPTANCE} alone: {selection}")
    expect(not query.calls, f"test/** alone queried bazel: {query.calls}")
    checks += 1

    selection, query = fake_select({"src/main.rs"})
    expect(query.calls[0] == ("set(//:src/main.rs)", True), f"root-package label is not //:src/main.rs: {query.calls}")
    expect(selection.targets == ["//:ocx_mirror_test"], f"root-package file selected {selection.targets}")
    checks += 1

    selection, query = fake_select({"crates/ocx_mirror_http/src/gone.rs"})
    expect(query.calls[0] == ("set(//crates/ocx_mirror_http:all)", True), f"deleted file not mapped to :all: {query.calls}")
    expect(selection.dropped == [], f"a deleted file's :all was reported dropped: {selection.dropped}")
    expect(selection.targets == ["//crates/ocx_mirror_http:ocx_mirror_http_test"], f"deleted file selected {selection.targets}")
    checks += 1

    selection, query = fake_select({"docs/index.md", "crates/ocx_mirror_http/src/lib.rs"})
    expect(selection.dropped == ["//:docs/index.md"], f"non-target not dropped: {selection.dropped}")
    expect(
        query.calls[1] == ("kind(test, rdeps(//..., set(//crates/ocx_mirror_http:src/lib.rs)))", False),
        f"rdeps saw a dropped label: {query.calls}",
    )
    checks += 1

    selection, query = fake_select({"docs/index.md"})
    expect(selection == Selection([], [], ["//:docs/index.md"]), f"docs-only change selected {selection}")
    expect(len(query.calls) == 1, f"an empty resolve still ran rdeps: {query.calls}")
    checks += 1

    for returncode, keep_going, red in [(0, True, False), (3, True, False), (1, True, True), (2, True, True), (3, False, True), (1, False, True)]:
        try:
            got = bazel_query(["bazel"], "set(//:a)", keep_going, run=fake_run(returncode))
        except SystemExit:
            got = None
        expect((got is None) == red, f"query exit {returncode} (keep_going={keep_going}) {'passed' if red else 'red'}")
        expect(red or got == {"//:a"}, f"query exit {returncode} lost its labels: {got}")
        checks += 1

    print(f"bazel scoped self-test: {checks} checks passed — every escalation trigger, test/**, root and deleted labels, dropped non-targets, a crashed query red")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--base")
    parser.add_argument("--out", type=Path)
    parser.add_argument("--bazel", help="the Bazel command, shell-quoted")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    if not (args.base and args.out and args.bazel):
        parser.error("--base, --out and --bazel are required")
    return run(args.base, args.out, shlex.split(args.bazel))


if __name__ == "__main__":
    sys.exit(main())
