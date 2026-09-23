# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
"""`//scripts:bazel_cache_rc_selftest` — `.github/actions/bazel-cache-rc/selftest.sh` under Bazel.

The selftest is ocx's, verbatim, and runs every Python step as `python3` off
PATH — `import yaml` included. Left alone under Bazel that is the host's
interpreter and the host's PyYAML (or none): an undeclared input deciding a
remotely cached verdict. So this runner puts one `python3` first on PATH that
is this test's own hermetic interpreter with this test's `sys.path` — the
locked `@scripts_pip//pyyaml` — and hands over to `bash selftest.sh`. The
script is not edited; the port stays re-syncable by copying.

`bash` and the coreutils it calls stay the host's, as for any `sh_test`.
Not a gate script: it has no mode but this one.
"""

import os
import shlex
import subprocess
import sys
from pathlib import Path

SELFTEST = Path(".github/actions/bazel-cache-rc/selftest.sh")


def main() -> int:
    bin_dir = Path(os.environ["TEST_TMPDIR"]) / "python-bin"
    bin_dir.mkdir()
    shim = bin_dir / "python3"
    # PYTHONPATH, not a symlink: the children must see the locked PyYAML,
    # which only this process's sys.path names.
    shim.write_text(
        "#!/bin/sh\n"
        f"PYTHONPATH={shlex.quote(os.pathsep.join(sys.path))} exec {shlex.quote(sys.executable)} \"$@\"\n",
        encoding="utf-8",
    )
    shim.chmod(0o755)
    env = {**os.environ, "PATH": f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}"}
    return subprocess.run(["bash", str(SELFTEST)], env=env, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
