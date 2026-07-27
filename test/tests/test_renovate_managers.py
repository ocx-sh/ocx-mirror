"""Every Renovate customManager must match something.

A customManager fails silently: a stale `managerFilePatterns`, a regex broken
by a rename, or a renamed capture group all produce the same observable as a
manager with nothing to do — nothing at all. Renovate cannot report "found no
files", because finding no files is legitimate.

That is not hypothetical. Ours pointed at `src/command/pipeline/generate/
templates/`, a path missing the `package/` level, and bumped no action pin for
as long as that level has existed.
"""
from __future__ import annotations

import json
import re
from pathlib import Path

import pytest

PROJECT_ROOT = Path(__file__).resolve().parent.parent.parent
RENOVATE_JSON = PROJECT_ROOT / "renovate.json"


def _managers() -> list[dict]:
    return json.loads(RENOVATE_JSON.read_text()).get("customManagers", [])


@pytest.mark.parametrize("manager", _managers(), ids=lambda m: m.get("description", "?")[:40])
def test_custom_manager_matches_something(manager: dict) -> None:
    # Renovate wraps file patterns in slashes and speaks JS named groups.
    file_re = re.compile(manager["managerFilePatterns"][0].strip("/"))
    content_res = [re.compile(s.replace("(?<", "(?P<")) for s in manager["matchStrings"]]

    matched = [
        p
        for p in PROJECT_ROOT.rglob("*")
        if p.is_file()
        and file_re.search(p.relative_to(PROJECT_ROOT).as_posix())
        and any(r.search(p.read_text(errors="ignore")) for r in content_res)
    ]

    assert matched, (
        f"Renovate customManager matches nothing: {manager.get('description')!r}\n"
        f"  files:  {manager['managerFilePatterns']}\n"
        f"  regex:  {manager['matchStrings']}\n"
        "This manager has been bumping nothing. Fix the pattern, or delete it."
    )
