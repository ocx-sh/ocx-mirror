#!/usr/bin/env bash
# Runner of //test:acceptance (adr_bazel_crate_split.md § C7). Runs the suite in
# the SOURCE tree against the Bazel-built ocx-mirror and the ocx.lock-pinned
# ocx — never conftest's fallbacks (test/bin, a cargo build of external/ocx),
# which are inputs this target does not declare.
#
# argv: <rlocationpath //:ocx-mirror> <rlocationpath @tools//:ocx> <rlocationpath @tools//:uv>
set -euo pipefail

fail() {
    echo "acceptance sh_test: $*" >&2
    exit 1
}

[ "$#" -eq 3 ] || fail "usage: <ocx-mirror> <ocx> <uv> rlocationpaths, got $# argument(s)"
for v in TEST_SRCDIR TEST_WORKSPACE TEST_TMPDIR XML_OUTPUT_FILE HOME; do
    [ -n "${!v:-}" ] || fail "$v is unset - run this through \`bazel test //test:acceptance\`"
done

runfile() {
    local p="$TEST_SRCDIR/$1"
    [ -x "$p" ] || fail "no executable runfile $1 at $p"
    readlink -f "$p"
}

mirror=$(runfile "$1")
uv=$(runfile "$3")

# @tools//:ocx is a rules_ocx launcher that exports OCX_HOME=<host store>
# before exec'ing the pinned binary. The suite hands every ocx it spawns a
# per-test OCX_HOME, which the launcher would overwrite — so the suite gets the
# binary the launcher execs, read off its single `exec '<path>' "$@"` line.
launcher=$(runfile "$2")
ocx=$(sed -n "s/^exec '\\([^']*\\)' \"\\\$@\"\$/\\1/p" "$launcher")
[ "$(printf '%s\n' "$ocx" | grep -c .)" = 1 ] && [ -x "$ocx" ] ||
    fail "cannot read the pinned ocx binary off the @tools//:ocx launcher $launcher (expected one exec '<path>' \"\$@\" line) - rules_ocx changed its launcher shape"

# The suite resolves its paths through `__file__`, so it runs in the source
# tree: under `no-sandbox` the conftest.py runfile is a symlink into it.
anchor="$TEST_SRCDIR/$TEST_WORKSPACE/test/conftest.py"
[ -e "$anchor" ] || fail "no runfile at $anchor - conftest.py is not in this target's data"
suite=$(dirname "$(readlink -f "$anchor")")
case "$suite" in
    *.runfiles/* | */bazel-out/*) fail "runfiles are copies here, so $suite is not the source tree (the target must stay no-sandbox)" ;;
esac
[ -f "$suite/pyproject.toml" ] || fail "$suite carries no pyproject.toml - wrong tree"

export OCX_MIRROR_COMMAND="$mirror" OCX_COMMAND="$ocx" OCX_TEST_BINARY="$ocx"

# The venv is per run, under TEST_TMPDIR: never in the source tree (test/.venv
# belongs to `task test:parallel`), never shared between checkouts or
# concurrent runs, and uv.lock (a declared input) fully determines it. uv's
# cache stays at its HOME default, so rebuilding it is a hardlink pass.
# `--locked` refuses a stale uv.lock instead of rewriting a declared input.
export UV_PROJECT_ENVIRONMENT="$TEST_TMPDIR/venv"

# pytest's arenas NOT under TEST_TMPDIR: Bazel symlinks the workspace .git into
# the execroot that holds it, and the mirror walks up to the nearest .git
# (SpecSlot) — a tmp_path inside a repository turns renderer tests red. Same
# disk-backed, repo-free root the suite uses outside Bazel.
TMPDIR="$HOME/.cache/ocx-mirror-test-tmp"
mkdir -p "$TMPDIR"
export TMPDIR

cd "$suite"
exec "$uv" run --locked pytest -n auto --junit-xml="$XML_OUTPUT_FILE"
