#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Floor and ceilings over a directory of acceptance JUnit reports.

    python3 scripts/accept_floor.py --junit target/bazel/accept     # bazel:test:accept (unsharded)
    python3 scripts/accept_floor.py --junit target/accept/junit     # test:accept:floor, merged shards

Sums testcases, skips and xfails over every `*.xml` under `--junit`; fails on a failure or error, on
fewer cases than `test/SUITE_FLOOR`, and on more skips / xfails than `test/SKIP_CEILING` /
`test/XFAIL_CEILING`. No report is red: reading nothing is not a clean suite. A shard holds a subset,
so the floor is judged over the merged directory. When `SKIPS.md` sits in the limits dir (the release
legs', `test/limits/<os>-release/`), every skipped test id (`<classname>.<name>`) must be listed in it exactly, and a `*-release` limits dir must have the file. `conformance_floor.py --junit` is a separate call.
"""

from __future__ import annotations

import argparse
import re
import sys
import tempfile
import xml.etree.ElementTree as ET
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
LIMIT_FILES = ("SUITE_FLOOR", "SKIP_CEILING", "XFAIL_CEILING")
# One `- <classname>.<name>: <reason>` line per skipped test; the id runs to the first `: `.
SKIPS_LINE = re.compile(r"^- (.+?): ", re.MULTILINE)


def run(junit: Path, limits_dir: Path, expect_shards: int = 0) -> int:
    reports = sorted(junit.rglob("*.xml"))
    if not reports:
        print(f"accept floor: no *.xml report under {junit} — nothing was read", file=sys.stderr)
        return 1

    # Extra reports beyond the expected names are fine (the conformance file, `all.xml`).
    names = {r.name for r in reports}
    missing = [f"shard-{i}-of-{expect_shards}.xml" for i in range(expect_shards)]
    missing = [m for m in missing if m not in names]
    if missing:
        print(f"accept floor: missing shard report(s) under {junit}: {', '.join(missing)}", file=sys.stderr)
        return 1

    def limit(name: str) -> int:
        return int((limits_dir / name).read_text(encoding="utf-8").strip())

    cases = skipped = xfailed = bad = 0
    skipped_ids: list[str] = []
    for report in reports:
        for case in ET.parse(report).getroot().iter("testcase"):
            cases += 1
            if case.find("failure") is not None or case.find("error") is not None:
                bad += 1
            for mark in case.findall("skipped"):
                if mark.get("type") == "pytest.xfail":
                    xfailed += 1
                else:
                    skipped += 1
                    skipped_ids.append(f"{case.get('classname')}.{case.get('name')}")

    # A missing limit file is red too, but the counts print first: a platform's first run has no
    # limit files and commits them from these numbers.
    absent = [limits_dir / name for name in LIMIT_FILES if not (limits_dir / name).is_file()]
    if absent:
        print(
            f"accept floor: no limit file {', '.join(str(a) for a in absent)} — counted {cases} cases, "
            f"{skipped} skipped, {xfailed} xfailed, {bad} failed or errored; commit the limits from these",
            file=sys.stderr,
        )
        return 1

    problems = []
    if bad:
        problems.append(f"{bad} failed or errored case(s)")
    if cases < limit("SUITE_FLOOR"):
        problems.append(f"{cases} cases ran, test/SUITE_FLOOR is {limit('SUITE_FLOOR')} — the suite shrank")
    if skipped > limit("SKIP_CEILING"):
        problems.append(f"{skipped} skipped exceeds test/SKIP_CEILING ({limit('SKIP_CEILING')})")
    if xfailed > limit("XFAIL_CEILING"):
        problems.append(f"{xfailed} xfailed exceeds test/XFAIL_CEILING ({limit('XFAIL_CEILING')})")
    skips_md = limits_dir / "SKIPS.md"
    if skips_md.is_file():
        listed = {m.group(1) for m in SKIPS_LINE.finditer(skips_md.read_text(encoding="utf-8"))}
        unlisted = [i for i in skipped_ids if i not in listed]
        if unlisted:
            problems.append(f"skipped test(s) missing from {skips_md}: {', '.join(unlisted)}")
    elif limits_dir.name.endswith("-release"):
        problems.append(f"{skips_md} is required for a release limits dir and does not exist")
    if problems:
        print("accept floor: " + "; ".join(problems), file=sys.stderr)
        return 1
    print(
        f"accept floor: {len(reports)} report(s); {cases} cases (floor {limit('SUITE_FLOOR')}), "
        f"{skipped} skipped (ceiling {limit('SKIP_CEILING')}), {xfailed} xfailed (ceiling {limit('XFAIL_CEILING')})"
    )
    return 0


def self_test() -> int:
    """Red and green on inputs it builds: a clean run, a shrunk suite, a skip absent from SKIPS.md."""
    case = '<testcase classname="t.m" name="{}">{}</testcase>'
    skip = '<skipped type="pytest.skip" message="why"/>'
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "junit").mkdir()
        (root / "junit" / "all.xml").write_text(
            "<testsuites><testsuite>" + case.format("a", "") + case.format("b", skip) + "</testsuite></testsuites>",
            encoding="utf-8",
        )
        limits = root / "limits"
        limits.mkdir()
        for name, value in zip(LIMIT_FILES, ("2", "1", "0")):
            (limits / name).write_text(value + "\n", encoding="utf-8")
        junit = root / "junit"
        failures = []
        if run(junit, limits) != 0:
            failures.append("clean run without SKIPS.md was red")
        (limits / "SKIPS.md").write_text("- t.m.b: why\n", encoding="utf-8")
        if run(junit, limits) != 0:
            failures.append("listed skip was red")
        (limits / "SKIPS.md").write_text("- t.m.other: why\n", encoding="utf-8")
        if run(junit, limits) == 0:
            failures.append("skip missing from SKIPS.md was green")
        (limits / "SKIPS.md").write_text("- t.m.b2: prefix of nothing, superstring of the skipped id\n", encoding="utf-8")
        if run(junit, limits) == 0:
            failures.append("skip matched only as a substring of another listed id was green")
        (limits / "SKIPS.md").unlink()
        release = root / "windows-release"
        release.mkdir()
        for name, value in zip(LIMIT_FILES, ("2", "1", "0")):
            (release / name).write_text(value + "\n", encoding="utf-8")
        if run(junit, release) == 0:
            failures.append("release limits dir without SKIPS.md was green")
        (limits / "SUITE_FLOOR").write_text("3\n", encoding="utf-8")
        if run(junit, limits) == 0:
            failures.append("shrunk suite was green")
    for f in failures:
        print(f"accept_floor self-test: {f}", file=sys.stderr)
    print("accept_floor self-test: " + ("FAILED" if failures else "ok"))
    return 1 if failures else 0


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--junit", type=Path)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--limits-dir", type=Path, default=REPO_ROOT / "test")
    parser.add_argument(
        "--expect-shards", type=int, default=0, metavar="N", help="require shard-0-of-N.xml .. shard-<N-1>-of-N.xml"
    )
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    if args.junit is None:
        parser.error("--junit is required")
    return run(args.junit, args.limits_dir, args.expect_shards)


if __name__ == "__main__":
    raise SystemExit(main())
