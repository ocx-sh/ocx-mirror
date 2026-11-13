#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""Write the CycloneDX SBOM of `ocx-mirror` for one build triple.

    scripts/release_sbom.py --target TRIPLE --output PATH [--manifest-path Cargo.toml]

Runs `cargo cyclonedx --format json --target TRIPLE` on the root package and moves the JSON it
writes beside the manifest to PATH. `cargo-cyclonedx` comes from the PATH (`ocx exec -g release --`).

Exit codes: 0 written, 1 cargo-cyclonedx failed or wrote no file, 64 a bad argument.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--manifest-path", default="Cargo.toml", type=Path)
    args = parser.parse_args(argv)
    stem = f"ocx-mirror-{args.target}"
    cmd = [
        "cargo",
        "cyclonedx",
        "--format",
        "json",
        "--target",
        args.target,
        "--manifest-path",
        str(args.manifest_path),
        "--override-filename",
        stem,
    ]
    try:
        done = subprocess.run(cmd, check=False)
    except OSError as error:
        print(f"release_sbom: cannot run cargo: {error}", file=sys.stderr)
        return 1
    if done.returncode != 0:
        print(f"release_sbom: cargo cyclonedx exited {done.returncode}", file=sys.stderr)
        return 1
    produced = args.manifest_path.parent / f"{stem}.cdx.json"
    if not produced.is_file():
        print(f"release_sbom: cargo cyclonedx wrote no {produced}", file=sys.stderr)
        return 1
    args.output.parent.mkdir(parents=True, exist_ok=True)
    shutil.move(produced, args.output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
