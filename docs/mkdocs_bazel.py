"""mkdocs, run from the pinned `@docs_pip` closure (docs/BUILD.bazel).

Two callers, one entry point:

- `bazel run //docs:mkdocs -- serve` (`task docs:serve`): plain mkdocs CLI,
  from the checkout rather than the runfiles tree, so `mkdocs.yml` and the
  `pymdownx.snippets` base path (`.`) resolve against the sources being edited.
- `//docs:site`: `--tar <out> build ...` builds into a scratch directory and
  packs it into one deterministic archive — sorted entries, mtime 0, uid/gid
  0, fixed modes — through this interpreter's `tarfile`, not the host `tar`,
  so the archive bytes depend on nothing outside the declared inputs.
"""

import os
import sys
import tarfile
import tempfile
from pathlib import Path

from mkdocs.__main__ import cli


def _mkdocs(args: list[str]) -> int:
    try:
        cli.main(args=args, prog_name="mkdocs")
    except SystemExit as exit_:
        code = exit_.code
        return code if isinstance(code, int) else (0 if code is None else 1)
    return 0


def _pack(site: Path, out: Path) -> None:
    # Directories are implied by their files; an empty one carries no content
    # the site serves, and leaving them out keeps the entry list one sort.
    files = sorted(p for p in site.rglob("*") if p.is_file())
    if not any(p.suffix == ".html" for p in files):
        sys.exit(f"mkdocs_bazel: mkdocs exited 0 but rendered no page into {site}")
    with tarfile.open(out, "w", format=tarfile.GNU_FORMAT) as tar:
        for path in files:
            info = tarfile.TarInfo(path.relative_to(site).as_posix())
            info.size = path.stat().st_size
            info.mode = 0o644
            with path.open("rb") as body:
                tar.addfile(info, body)


def main() -> int:
    argv = sys.argv[1:]
    if argv[:1] != ["--tar"]:
        # `bazel run` starts in the runfiles tree and names the checkout here.
        workspace = os.environ.get("BUILD_WORKSPACE_DIRECTORY")
        if workspace:
            os.chdir(workspace)
        return _mkdocs(argv)

    if len(argv) < 3:
        sys.exit("usage: mkdocs_bazel.py --tar <out.tar> build [mkdocs build args]")
    out = Path(argv[1]).absolute()
    # mkdocs reads the epoch for sitemap.xml's <lastmod> and the gzip header
    # of sitemap.xml.gz; left unset both carry the wall clock.
    os.environ["SOURCE_DATE_EPOCH"] = "0"
    with tempfile.TemporaryDirectory() as scratch:
        site = Path(scratch) / "site"
        code = _mkdocs([*argv[2:], "--site-dir", str(site)])
        if code != 0:
            return code
        _pack(site, out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
