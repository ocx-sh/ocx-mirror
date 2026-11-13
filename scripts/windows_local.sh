#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 The OCX Authors
# Runs the acceptance suite on the Windows host from WSL:
# `task test:windows-local MODE=accept -- --exe <ocx-mirror.exe>`. `--help` prints the flags and the exit codes.
# Ported from ocx's scripts/windows_local.sh; the cross-build default and the unit-bundle mode are not ported.

set -euo pipefail

# The suite starts its own zot registries on free ports. The registry:2 mirror is Docker
# only, and WSL forwards its own localhost:5001 to Windows, so the default port would
# reach WSL's compose stack; a dedicated one is reachable by nothing.
PORT_MIRROR=15001
GIT_USR_BIN='C:\Program Files\Git\usr\bin'
STAGE_DIR=ocx-mirror-wintest

usage() {
    cat <<'EOF2'
windows_local.sh --mode accept --exe PATH [--shard-count N --shard-index I] [-- ARGS...]

accept: ARGS go to pytest through `task test:accept:native`; --exe is the Windows
        ocx-mirror.exe under test (required: no cross-build default yet).

Exit: the child's status; 64 usage; 66 --exe missing or not a file;
69 not WSL with a cmd.exe; 77 a staging directory that is a link or resolves outside ocx-mirror-wintest;
2 a shard request accept_shard.py refuses; 74 a passing run whose reports could not be copied back.
EOF2
}

die() {
    local status=$1
    shift
    printf 'windows_local: %s\n' "$*" >&2
    [ "$status" -ne 64 ] || usage >&2
    exit "$status"
}

# Sets QUOTED to "$1" for a line of a batch file. cmd.exe has no escape for a `"` inside quotes and a line break
# ends the line, so either is refused; `%` expands even inside quotes, so it is doubled.
batch_quote() {
    case $1 in
        *\"* | *$'\n'* | *$'\r'*) die 64 "'$1' holds a double quote or a line break, which cmd.exe cannot carry" ;;
    esac
    QUOTED=\"${1//\%/%%}\"
}

mode=accept shard_count='' shard_index='' exe=''
while [ $# -gt 0 ]; do
    case $1 in
        --mode | --shard-count | --shard-index | --exe)
            [ $# -ge 2 ] || die 64 "$1 needs a value"
            case $1 in
                --mode) mode=$2 ;;
                --shard-count) shard_count=$2 ;;
                --shard-index) shard_index=$2 ;;
                --exe) exe=$2 ;;
            esac
            shift 2
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        *) die 64 "unknown argument '$1'" ;;
    esac
done
args=''
for a in "$@"; do
    batch_quote "$a"
    args="$args $QUOTED"
done

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)

[ "$mode" = accept ] || die 64 "--mode is accept, got '$mode'"
# The selector is the authority on what a shard request means (exit 2), and it answers
# before any staging is paid for.
if [ -n "$shard_count$shard_index" ]; then
    select_flags=()
    [ -z "$shard_count" ] || select_flags+=(--shard-count "$shard_count")
    [ -z "$shard_index" ] || select_flags+=(--shard-index "$shard_index")
    status=0
    python3 "$ROOT/scripts/accept_shard.py" --format stems "${select_flags[@]}" >/dev/null || status=$?
    [ "$status" -eq 0 ] || exit "$status"
fi
[ -n "$exe" ] || die 66 "--exe PATH is required: there is no cross-build default yet"
[ -f "$exe" ] || die 66 "--exe '$exe' is not a file"

# WINDOWS_LOCAL_CMD_EXE names another cmd.exe (how a test reaches 69) and WINDOWS_LOCAL_DRIVE_ROOT the
# directory it starts in (default /mnt/c).
CMD_EXE=${WINDOWS_LOCAL_CMD_EXE:-$(command -v cmd.exe || echo /mnt/c/Windows/System32/cmd.exe)}
{ [ -n "${WSL_DISTRO_NAME:-}" ] || grep -qi microsoft /proc/version 2>/dev/null; } && [ -x "$CMD_EXE" ] ||
    die 69 "needs WSL with a Windows cmd.exe (looked for $CMD_EXE)"

NAME=$(basename "$ROOT")

# cmd.exe starts in a drive directory: from a \\wsl$ cwd it prints a warning and
# falls back to the Windows directory.
win_profile=$(cd "${WINDOWS_LOCAL_DRIVE_ROOT:-/mnt/c}" && "$CMD_EXE" /c 'echo %USERPROFILE%' 2>/dev/null | tr -d '\r') ||
    die 69 "cmd.exe did not answer"
[ -n "$win_profile" ] || die 69 "cmd.exe reported no %USERPROFILE%"
base_wsl="$(wslpath -u "$win_profile")/$STAGE_DIR"
stage_wsl="$base_wsl/$NAME"
stage_win="$win_profile\\$STAGE_DIR\\$NAME"
# Owned by this script: the python3 venv, the env file, pytest's scratch. Never in the
# stage (the sweep below deletes whatever git does not list). Short names on purpose:
# deep paths under pytest's basetemp reach MAX_PATH.
own_wsl="$base_wsl/_wl"
own_win="$win_profile\\$STAGE_DIR\\_wl"
env_win="$own_win\\$NAME-env.cmd"

# Runs the command line $1 under cmd.exe from the stage, CRs stripped, the child's status kept
# (`task -x` hands pytest's own exit code through; plain `task` turns any failure into 201).
# Through a batch file: WSL interop escapes every double quote in an argument with a backslash,
# so a quoted path in `cmd.exe /c "..."` arrives as \"path\" and cmd.exe cannot find it.
win() {
    {
        printf '%s\r\n' '@echo off' "call \"$env_win\"" "cd /d \"$stage_win\"" "$1" 'exit /b %ERRORLEVEL%'
    } >"$own_wsl/$NAME-step.cmd"
    (cd "$stage_wsl" && "$CMD_EXE" /c "$own_win\\$NAME-step.cmd" 2>&1 </dev/null | tr -d '\r')
}

# The working tree on C:. Windows processes cannot use a \\wsl$ cwd. git decides what
# the tree is; rsync cannot both take a file list and delete, so files git no longer
# lists are swept first. Kept across runs: the venv, caches, test/bin, target/.
mkdir -p "$stage_wsl" "$own_wsl"
# The sweep below deletes whatever git does not list, so a stage that resolves anywhere but
# <profile>/ocx-mirror-wintest/<name> (a link, a junction WSL reports as one) would delete from where it points.
profile_real=$(cd "$(wslpath -u "$win_profile")" && pwd -P)
for dir in "$stage_wsl" "$own_wsl"; do
    [ "$(cd "$dir" && pwd -P)" = "$profile_real/$STAGE_DIR/$(basename "$dir")" ] ||
        die 77 "$dir is a link or resolves outside $profile_real/$STAGE_DIR; remove it"
done
list=$(mktemp "${TMPDIR:-/tmp}/windows-local.XXXXXX")
have=$(mktemp "${TMPDIR:-/tmp}/windows-local.XXXXXX")
gone=$(mktemp "${TMPDIR:-/tmp}/windows-local.XXXXXX")
trap 'rm -f "$list" "$have" "$gone"' EXIT
# external/ is the submodule checkout; .agents/ holds other tasks' worktrees. `ls-files -c` still names a
# tracked file deleted from the working tree, which rsync exits 23 on, so the deleted ones are subtracted. Each git
# call is the head of its own pipeline: a failing git must stop the script, not empty $list and sweep the stage.
(cd "$ROOT" && git ls-files -d -z) | sort -z >"$gone"
(cd "$ROOT" && git ls-files -co --exclude-standard -z) | sort -zu | comm -z -23 - "$gone" |
    { grep -zv -e '^external/' -e '^\.agents/' || [ $? -eq 1 ]; } >"$list"
# test_ocx_spawn_sites loads ocx's fake forge from the submodule's test tree, which the stage otherwise lacks.
(cd "$ROOT" && find external/ocx/test/tests -type f -not -name '*.pyc' -print0) >>"$list"
(cd "$stage_wsl" && find . \( -name .git -o -name .venv -o -name __pycache__ -o -name .pytest_cache -o -name .ruff_cache \) -prune \
    -o -path ./test/bin -prune -o -path ./target -prune -o -path ./test/results -prune -o -type f -print0) |
    sed -z 's|^\./||' >"$have"
comm -z -23 <(sort -z "$have") <(sort -z "$list") | (cd "$stage_wsl" && xargs -0 -r rm -f --)
rsync -rt --from0 --files-from="$list" "$ROOT/" "$stage_wsl/"
# A checkout, as the suite's main_checkout_root test expects; `.git` is kept across runs, not swept.
[ -d "$stage_wsl/.git" ] || git init -q "$stage_wsl"
mkdir -p "$stage_wsl/test/bin"
cp -f "$exe" "$stage_wsl/test/bin/ocx-mirror.exe"

# The environment every Windows command below runs under, as one file (CRLF).
# Git's usr\bin goes ahead of %PATH%, as acceptance.yml's $GITHUB_PATH line puts it: appended,
# a bare `bash` resolves to System32's WSL launcher instead of Git's, unlike on the CI runner.
{
    printf '%s\r\n' '@echo off'
    printf '%s\r\n' "set \"PATH=$own_win\\py\\Scripts;$GIT_USR_BIN;%PATH%\""
    printf '%s\r\n' 'if not defined HOME set "HOME=%USERPROFILE%"'
    printf '%s\r\n' 'set "PYTHONIOENCODING=utf-8"'
    # A host-global commit.gpgsign would break the fixtures' commits.
    printf '%s\r\n' 'set "GIT_CONFIG_COUNT=2"' 'set "GIT_CONFIG_KEY_0=commit.gpgsign"' 'set "GIT_CONFIG_VALUE_0=false"' \
        'set "GIT_CONFIG_KEY_1=tag.gpgsign"' 'set "GIT_CONFIG_VALUE_1=false"'
    # The Windows host has no Docker the suite can reach, as on the deep tier's Windows leg.
    printf '%s\r\n' 'set "SKIP_BUILD=true"' 'set "OCX_TEST_NO_DOCKER=1"' "set \"OCX_TEST_MIRROR_PORT=$PORT_MIRROR\""
    # pytest's scratch stays short: the mirror's index paths reach MAX_PATH (os error 3) under the default
    # %TEMP%\\pytest-of-<user>. One checkout at a time: pytest empties this directory at the start of a run.
    printf '%s\r\n' "set \"TEST_BASETEMP=$own_win\\p\""
    # An --exe that is release bytes lacks the __testing seams: the switch skips the marked tests.
    [ "${OCX_TEST_RELEASE_BINARY:-}" != 1 ] || printf '%s\r\n' 'set "OCX_TEST_RELEASE_BINARY=1"'
} >"$own_wsl/$NAME-env.cmd"

# python3, date, tr, dirname and basename: what test/taskfile.yml shells out to and a
# Windows PATH lacks. Git's usr\bin has the coreutils; python3 is a copy of python.exe
# inside a venv this script owns, because a copy beside it loses the DLLs it loads.
# WindowsApps\python3.exe is the Store stub, so the venv leads the PATH.
if [ ! -f "$own_wsl/py/Scripts/python3.exe" ]; then
    win "uv venv \"$own_win\\py\" --python 3.14 --allow-existing"
    cp -f "$own_wsl/py/Scripts/python.exe" "$own_wsl/py/Scripts/python3.exe"
fi

# The run. zot comes from ocx.lock through the Windows host's ocx; pytest starts it itself.
rc=0
shard_vars=''
[ -z "$shard_count" ] || shard_vars="$shard_vars SHARD_COUNT=$shard_count"
[ -z "$shard_index" ] || shard_vars="$shard_vars SHARD_INDEX=$shard_index"
win "ocx exec zot -- task -x test:accept:native$shard_vars -- $args" || rc=$?
# The reports leave with the run: the merge job's limits are counted off them.
mkdir -p "$ROOT/target/windows-local/junit"
if [ -d "$stage_wsl/target/accept/junit" ]; then
    # A report that cannot leave the stage is said so and ends the run in 74, unless the run already failed.
    cp -f "$stage_wsl"/target/accept/junit/*.xml "$ROOT/target/windows-local/junit/" || {
        printf 'windows_local: could not copy the reports out of the stage (cp exit %s)\n' "$?" >&2
        [ "$rc" -ne 0 ] || rc=74
    }
fi
exit "$rc"
