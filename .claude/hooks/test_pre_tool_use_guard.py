"""Tests for pre_tool_use_guard.py — scenarios S-001…S-042 of
.claude/state/plans/plan_bazel_phase0_ai_config.md (ADR adr_bazel_crate_split.md C9(c)).

The hook is driven through its real interface: `python3 <hook>` with the
PreToolUse JSON on stdin and CLAUDE_PROJECT_DIR set. The fixture is a throwaway
layout with a real submodule, built once per session; each test that depends on
the submodule's HEAD state sets it explicitly.
"""

import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

import pytest

HOOK_DIR = Path(__file__).resolve().parent
HOOK = HOOK_DIR / "pre_tool_use_guard.py"
REPO_ROOT = Path(__file__).resolve().parents[2]
TOPLEVEL = '"$(git rev-parse --show-toplevel)/external/ocx"'


# --------------------------------------------------------------------------- fixture


def _git(*args, cwd):
    subprocess.run(
        [
            "git",
            "-c", "commit.gpgsign=false",
            "-c", "protocol.file.allow=always",
            "-c", "core.hooksPath=/dev/null",
            *args,
        ],
        cwd=cwd,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )


def _init_repo(path):
    path.mkdir(parents=True)
    _git("init", "-q", "-b", "main", cwd=path)
    _git("config", "user.name", "guard-test", cwd=path)
    _git("config", "user.email", "guard-test@example.invalid", cwd=path)
    (path / "README.md").write_text("fixture\n", encoding="utf-8")
    _git("add", ".", cwd=path)
    _git("commit", "-q", "-m", "init", cwd=path)


@dataclass
class Layout:
    base: Path
    mirror: Path
    sub: Path
    sibling: Path
    other: Path


@pytest.fixture(scope="session")
def layout(tmp_path_factory):
    base = tmp_path_factory.mktemp("guard").resolve()
    probe = subprocess.run(
        ["git", "-C", str(base), "rev-parse", "--git-dir"], capture_output=True
    )
    assert probe.returncode != 0, (
        f"{base} is inside a git repository — set TMPDIR outside any repo"
    )

    nested_up = base / "upstream" / "nested"
    ocx_up = base / "upstream" / "ocx"
    _init_repo(nested_up)
    _init_repo(ocx_up)
    _git("submodule", "add", "-q", str(nested_up), "external/nested", cwd=ocx_up)
    _git("commit", "-q", "-m", "nested", cwd=ocx_up)

    mirror = base / "dev" / "ocx-mirror"
    _init_repo(mirror)
    _git("submodule", "add", "-q", str(ocx_up), "external/ocx", cwd=mirror)
    _git("commit", "-q", "-m", "ocx", cwd=mirror)
    sub = mirror / "external" / "ocx"
    _git("submodule", "update", "-q", "--init", cwd=sub)
    (mirror / "sublink").symlink_to(sub)

    sibling = base / "dev" / "ocx"
    other = base / "dev" / "other"
    _init_repo(sibling)
    _init_repo(other)
    return Layout(base, mirror, sub, sibling, other)


@pytest.fixture(scope="session")
def worktree(layout):
    """Linked worktree of the fixture mirror, inside it like `.agents/worktrees/`."""
    wt = layout.mirror / ".agents" / "worktrees" / "wt1"
    _git("worktree", "add", "-q", str(wt), "-b", "wt1", cwd=layout.mirror)
    _git("submodule", "update", "-q", "--init", "external/ocx", cwd=wt)
    return wt


def set_head(layout, state):
    if state == "main":
        _git("checkout", "-q", "main", cwd=layout.sub)
    elif state == "detached":
        _git("checkout", "-q", "--detach", "main", cwd=layout.sub)
    else:
        _git("checkout", "-q", "-B", "feature/x", "main", cwd=layout.sub)


# --------------------------------------------------------------------------- helpers


def run_hook(layout, stdin, env=None):
    return subprocess.run(
        ["python3", str(HOOK)],
        input=stdin if isinstance(stdin, str) else json.dumps(stdin),
        env={**os.environ, "CLAUDE_PROJECT_DIR": str(layout.mirror), **(env or {})},
        capture_output=True,
        text=True,
        encoding="utf-8",
        timeout=30,
    )


def bash(layout, command, cwd=None, env=None):
    payload = {
        "tool_name": "Bash",
        "tool_input": {"command": command},
        "cwd": str(cwd or layout.mirror),
    }
    return run_hook(layout, payload, env)


def edit(layout, path, tool="Write", cwd=None, env=None):
    key = "notebook_path" if tool == "NotebookEdit" else "file_path"
    payload = {"tool_name": tool, "tool_input": {key: str(path)},
               "cwd": str(cwd or layout.mirror)}
    return run_hook(layout, payload, env)


def assert_blocked(result, rule):
    assert result.returncode == 2, (result.returncode, result.stderr)
    assert result.stderr.startswith(f"ocx-mirror guard {rule}: "), result.stderr
    assert " — use " in result.stderr, result.stderr
    assert result.stderr.count("\n") <= 1, result.stderr


def assert_allowed(result):
    assert (result.returncode, result.stderr) == (0, "")


# --------------------------------------------------------------------------- G1


@pytest.mark.parametrize(
    ("sid", "state", "cmd"),
    [
        ("S-001", "main", "git -C {sub} commit -m x"),
        ("S-002", "detached", "git -C {sub} commit -m x"),
        ("S-003", "detached", "git -C {sub} push origin x"),
        ("G1-merge", "main", "git -C {sub} merge origin/main"),
        ("G1-pull", "detached", "git -C {sub} pull"),
        ("G1-timeout", "main", "timeout 60 git -C {sub} commit -m x"),
        ("G1-timeout-opts", "main", "timeout -k 5 --foreground 60s git -C {sub} commit -m x"),
    ],
)
def test_g1_blocks_history_writes_on_main_or_detached(layout, sid, state, cmd):
    set_head(layout, state)
    assert_blocked(bash(layout, cmd.format(sub=layout.sub)), "G1")


@pytest.mark.parametrize("cmd", ["git -C {sub} commit -m x", "git -C {sub} push origin x"])
def test_s030_g1_allows_feature_branch(layout, cmd):
    set_head(layout, "feature")
    assert_allowed(bash(layout, cmd.format(sub=layout.sub)))


@pytest.mark.parametrize(
    "move",
    ["switch main", "checkout --detach", "switch feat/existing", "checkout main"],
)
def test_g1_blocks_history_write_after_head_move_in_same_command(layout, move):
    set_head(layout, "feature")
    cmd = f"git -C {TOPLEVEL} {move} && git -C {TOPLEVEL} commit -m x"
    result = bash(layout, cmd)
    assert_blocked(result, "G1")
    assert "as its own command first" in result.stderr, result.stderr


@pytest.mark.parametrize(
    "move",
    ["switch -c feat/y", "switch --create feat/y", "checkout -b feat/y",
     "checkout -- README.md", "switch main && git -C {top} switch -C feat/y"],
)
def test_g1_allows_history_write_after_branch_creation(layout, move):
    set_head(layout, "feature")
    move = move.replace("{top}", TOPLEVEL)
    assert_allowed(bash(layout, f"git -C {TOPLEVEL} {move} && git -C {TOPLEVEL} commit -m x"))


@pytest.mark.parametrize(
    "cmd",
    [
        'git -C "$(pwd)/external/ocx" commit -m x',
        'git -C "$PWD/external/ocx" commit -m x',
        'git -C "${PWD}/external/ocx" commit -m x',
        'git -C "`pwd`/external/ocx" commit -m x',
        "git -C $(pwd)/external/ocx commit -m x",
        "git -C `git rev-parse --show-toplevel`/external/ocx commit -m x",
        "git -C $(git rev-parse --show-toplevel)/external/ocx commit -m x",
        'cd external && git -C "$PWD/ocx" commit -m x',
        "ocx run -- git -C {sub} commit -m x",
        "ocx run -p pkg -- git -C {sub} commit -m x",
        "ocx exec pkg -- git -C {sub} commit -m x",
        "uv run -- git -C {sub} commit -m x",
    ],
)
def test_g1_sees_through_pwd_backticks_and_wrappers(layout, cmd):
    set_head(layout, "detached")
    try:
        assert_blocked(bash(layout, cmd.replace("{sub}", str(layout.sub))), "G1")
    finally:
        set_head(layout, "feature")  # the G2 `timeout … commit` case assumes a writable HEAD


@pytest.mark.parametrize(
    "cmd",
    ['git -C "$(pwd)/external/ocx" log', 'git -C "$PWD/external/ocx" log',
     "git -C `git rev-parse --show-toplevel`/external/ocx log"],
)
def test_pwd_and_backtick_c_count_as_absolute(layout, cmd):
    assert_allowed(bash(layout, cmd))


@pytest.mark.parametrize(
    "cmd",
    ['git -C "$PWD" commit -m x', 'git -C "${PWD}" commit -m x',
     'git -C "$(pwd)" push', 'git -C "`pwd`" log',
     'git -C "$(git rev-parse --show-toplevel)" commit -m x'],
)
def test_g2_cwd_spelling_from_inside_submodule_is_not_absolute(layout, cmd):
    set_head(layout, "feature")  # G2, not G1, must be what blocks
    result = bash(layout, cmd, cwd=layout.sub)
    assert_blocked(result, "G2")
    assert f"first cd {layout.mirror}" in result.stderr, result.stderr


@pytest.mark.parametrize("state", ["main", "detached"])
@pytest.mark.parametrize(
    "create", ["switch -c feat/x", "checkout -b feat/x", "switch main && git -C {top} switch -c feat/x"],
)
def test_g1_allows_write_chained_with_and_after_branch_creation(layout, state, create):
    set_head(layout, state)
    try:
        create = create.replace("{top}", TOPLEVEL)
        assert_allowed(bash(layout, f"git -C {TOPLEVEL} {create} && echo ok && "
                                    f"git -C {TOPLEVEL} commit -m x"))
    finally:
        set_head(layout, "feature")


@pytest.mark.parametrize("sep", [" ; ", "\n", " || ", " | cat && ", " & "])
def test_g1_ignores_branch_creation_across_a_non_and_connector(layout, sep):
    set_head(layout, "main")
    try:
        cmd = f"git -C {TOPLEVEL} switch -c feat/x{sep}git -C {TOPLEVEL} commit -m x"
        assert_blocked(bash(layout, cmd), "G1")
    finally:
        set_head(layout, "feature")


@pytest.mark.parametrize("state", ["main", "detached", "feature"])
@pytest.mark.parametrize(
    "create",
    ["switch -C main", "checkout -B main", "switch -c main origin/main",
     "switch --force-create=main", "switch --create main", "checkout -bmain",
     "switch --orphan main", "switch -c $BRANCH", "switch -c",
     "checkout -B main --", "switch -C main --", "checkout main --",
     "branch -M main", "branch -m feature/x main"],
)
def test_g1_blocks_write_after_creating_main_or_an_unknown_branch(layout, state, create):
    set_head(layout, state)
    try:
        result = bash(layout, f"git -C {TOPLEVEL} {create} && git -C {TOPLEVEL} commit -m x")
        assert_blocked(result, "G1")
        assert "as its own command first" in result.stderr, result.stderr
    finally:
        set_head(layout, "feature")


@pytest.mark.parametrize("sep", [" &&\n", " && \n", "&&\n\n", " &&\n  \n"])
def test_g1_and_chain_survives_a_newline_after_the_connector(layout, sep):
    set_head(layout, "main")
    try:
        cmd = f"git -C {TOPLEVEL} switch -c feat/x{sep}git -C {TOPLEVEL} commit -m x"
        assert_allowed(bash(layout, cmd))
    finally:
        set_head(layout, "feature")


@pytest.mark.parametrize("sep", [" ||\n", " |\n cat\n", " &&\n echo ok\n"])
def test_g1_newline_after_non_and_or_between_commands_still_breaks_the_chain(layout, sep):
    set_head(layout, "main")
    try:
        cmd = f"git -C {TOPLEVEL} switch -c feat/x{sep}git -C {TOPLEVEL} commit -m x"
        assert_blocked(bash(layout, cmd), "G1")
    finally:
        set_head(layout, "feature")


def test_g1_failed_creation_after_a_move_stays_unknowable(layout):
    set_head(layout, "feature")
    cmd = (f"git -C {TOPLEVEL} switch main && git -C {TOPLEVEL} switch -c feat/x ; "
           f"git -C {TOPLEVEL} commit -m x")
    result = bash(layout, cmd)
    assert_blocked(result, "G1")
    assert "as its own command first" in result.stderr, result.stderr


# --------------------------------------------------------------------------- G2


@pytest.mark.parametrize(
    ("sid", "cmd"),
    [
        ("S-004", "cd external/ocx && git status"),
        ("S-005", "git -C external/ocx log"),
        ("S-007", "cd external/ocx\ngit log"),
        ("S-008", "FOO=1 git -C external/ocx status"),
        ("S-008-env", "env X=1 git -C external/ocx status"),
        ("S-009", "rtk git -C external/ocx status"),
        ("S-009-proxy", "rtk proxy git -C external/ocx status"),
        ("S-009-abs-git", "/usr/bin/git -C external/ocx status"),
        ("S-010", 'cd "external/ocx" ; git fetch'),
        ("S-010-single", "git -C 'external/ocx' log"),
        ("S-011", "cd external\ncd ocx && git log"),
        ("chain-and", "git status && cd external/ocx && git log"),
        ("chain-or", "false || git -C external/ocx log"),
        ("chain-semi-pipe", "echo a; git -C external/ocx log | head"),
        ("subshell", "(cd external/ocx && git status)"),
        ("subshell-after-cd", "cd external/ocx && (git status)"),
        ("subshell-nested", "(cd external/ocx; (cd .. && ls); git status)"),
        ("timeout", "timeout 60 git -C external/ocx commit -m x"),
        ("for-loop", "for c in a b; do git -C external/ocx log; done"),
        ("composed-C", "git -C external -C ocx log"),
        ("symlink-cd", "cd sublink && git status"),
        ("quoted-hash-not-comment", "echo '#' ; git -C external/ocx log"),
        ("continuation", "git -C external/ocx \\\n  log"),
        ("nested-submodule-cd", "cd external/ocx/external/nested && git status"),
    ],
)
def test_g2_blocks_git_without_absolute_c(layout, sid, cmd):
    assert_blocked(bash(layout, cmd), "G2")


def test_s006_g2_blocks_session_cwd_inside_submodule(layout):
    result = bash(layout, "git status", cwd=layout.sub)
    assert_blocked(result, "G2")
    assert f"first cd {layout.mirror}" in result.stderr, result.stderr


def test_s031_absolute_c_log_allowed_even_on_main(layout):
    set_head(layout, "main")
    assert_allowed(bash(layout, f"git -C {layout.sub} log"))


@pytest.mark.parametrize(
    "cmd",
    [
        f"git -C {TOPLEVEL} fetch origin --tags",
        f"git -C {TOPLEVEL} log --oneline HEAD~0..HEAD",
        f'for c in a b; do git -C {TOPLEVEL} diff --stat HEAD -- crates/$c; done',
        f"NEW=$(git -C {TOPLEVEL} rev-parse HEAD)",
        'git -C "{sub}" log',
    ],
)
def test_s032_show_toplevel_c_allowed(layout, cmd):
    assert_allowed(bash(layout, cmd.replace("{sub}", str(layout.sub))))


def test_s033_cd_into_submodule_for_non_git_allowed(layout):
    assert_allowed(bash(layout, "cd external/ocx && cargo build"))


@pytest.mark.parametrize(
    "cmd",
    [
        "(cd external/ocx && cargo build) && git status",
        "(cd external/ocx && cargo build)\ngit status",
        "case $x in a) git status;; esac",
    ],
)
def test_subshell_cd_does_not_leak(layout, cmd):
    assert_allowed(bash(layout, cmd))


def test_s042_nested_submodule_absolute_allowed(layout):
    assert_allowed(bash(layout, f"git -C {layout.sub}/external/nested status"))


# --------------------------------------------------------------------------- G3


def test_s012_g3_blocks_recursive_update_from_root(layout):
    assert_blocked(bash(layout, "git submodule update --init --recursive"), "G3")


def test_s013_g3_blocks_recursive_update_with_root_c(layout):
    assert_blocked(bash(layout, f"git -C {layout.mirror} submodule update --recursive"), "G3")


def test_g3_blocks_option_before_update(layout):
    assert_blocked(bash(layout, "git submodule --quiet update --init --recursive"), "G3")


def test_g3_blocks_recursive_update_in_fresh_worktree_root(layout):
    cmd = "git -C .agents/worktrees/wt9 submodule update --init --recursive"
    assert_blocked(bash(layout, cmd), "G3")


@pytest.mark.parametrize("tail", ["external/ocx", "external/ocx/", "external/ocx/external/nested"])
def test_g3_allows_update_in_not_yet_created_worktree_submodule(layout, tail):
    cmd = ("git worktree add .agents/worktrees/wt9 -b wt9 && git -C "
           f'"$(git rev-parse --show-toplevel)/.agents/worktrees/wt9/{tail}" '
           "submodule update --init --recursive")
    assert_allowed(bash(layout, cmd))


def test_s034_recursive_update_inside_submodule_allowed(layout):
    assert_allowed(bash(layout, f"git -C {layout.sub} submodule update --init --recursive"))


def test_s035_non_recursive_update_from_root_allowed(layout):
    assert_allowed(bash(layout, f"git -C {layout.mirror} submodule update --init external/ocx"))


def test_s036_unrelated_repo_recursive_update_allowed(layout):
    assert_allowed(bash(layout, "git submodule update --init --recursive", cwd=layout.other))


# --------------------------------------------------------------------------- G4


@pytest.mark.parametrize(
    ("sid", "state", "tool", "rel"),
    [
        ("S-014", "detached", "Write", "external/ocx/Cargo.toml"),
        ("S-015", "main", "Edit", "external/ocx/Cargo.toml"),
        ("S-015-multi", "main", "MultiEdit", "external/ocx/Cargo.toml"),
        ("S-016", "detached", "NotebookEdit", "external/ocx/x.ipynb"),
        ("S-017", "detached", "Write", "sublink/Cargo.toml"),
        ("nested", "detached", "Write", "external/ocx/external/nested/README.md"),
    ],
)
def test_g4_blocks_edits_on_main_or_detached(layout, sid, state, tool, rel):
    set_head(layout, state)
    assert_blocked(edit(layout, layout.mirror / rel, tool), "G4")


@pytest.mark.parametrize("tool", ["Write", "Edit"])
def test_s037_edits_on_feature_branch_allowed(layout, tool):
    set_head(layout, "feature")
    assert_allowed(edit(layout, layout.sub / "Cargo.toml", tool))


# --------------------------------------------------------------------------- G5


def test_s018_g5_blocks_write_in_sibling(layout):
    assert_blocked(edit(layout, layout.sibling / "README.md"), "G5")


@pytest.mark.parametrize(
    ("sid", "cmd", "env"),
    [
        ("S-019", "git -C {sibling} commit -m x", None),
        ("S-020", "cd ../ocx && git checkout -b y", None),
        ("S-021", "git -C ../ocx merge z", None),
        ("home-var", 'git -C "$HOME/dev/ocx" merge z', "home"),
        ("tilde", "git -C ~/dev/ocx merge z", "home"),
    ],
)
def test_g5_blocks_git_writes_in_sibling(layout, sid, cmd, env):
    env = {"HOME": str(layout.base)} if env else None
    assert_blocked(bash(layout, cmd.format(sibling=layout.sibling), env=env), "G5")


@pytest.mark.parametrize(
    "cmd",
    [
        "git -C {sibling} log",
        "git -C {sibling} status",
        "git -C {sibling} diff",
        "git -C {sibling} show",
        "cd ../ocx && git status",
        "git -C ../ocx show-ref",
        "git -C ../ocx shortlog -s",
        "git -C ../ocx ls-remote origin",
        "git -C ../ocx name-rev HEAD",
        "git -C ../ocx cherry main",
        "git -C ../ocx range-diff a...b",
    ],
)
def test_s038_g5_reads_allowed(layout, cmd):
    assert_allowed(bash(layout, cmd.format(sibling=layout.sibling)))


def test_s038_read_tool_not_matched(layout):
    payload = {
        "tool_name": "Read",
        "tool_input": {"file_path": str(layout.sibling / "README.md")},
        "cwd": str(layout.mirror),
    }
    assert_allowed(run_hook(layout, payload))


def test_s038_prefix_trap_mirror_is_not_inside_sibling(layout):
    assert str(layout.mirror).startswith(str(layout.sibling))
    assert_allowed(edit(layout, layout.mirror / "src" / "x.rs"))


# --------------------------------------------------------------------------- worktree


def test_worktree_g2_relative_c_blocked(layout, worktree):
    env = {"CLAUDE_PROJECT_DIR": str(worktree)}
    assert_blocked(bash(layout, "git -C external/ocx log", cwd=worktree, env=env), "G2")


def test_worktree_g5_sibling_resolves_to_main_checkout(layout, worktree):
    env = {"CLAUDE_PROJECT_DIR": str(worktree)}
    rel = os.path.relpath(layout.sibling, worktree)
    assert_blocked(bash(layout, f"git -C {rel} merge z", cwd=worktree, env=env), "G5")
    assert_blocked(edit(layout, layout.sibling / "README.md", cwd=worktree, env=env), "G5")
    assert_allowed(bash(layout, f"git -C {rel} log", cwd=worktree, env=env))


def test_worktree_submodule_detached_g1_g4(layout, worktree):
    env = {"CLAUDE_PROJECT_DIR": str(worktree)}
    wsub = worktree / "external" / "ocx"
    _git("checkout", "-q", "--detach", cwd=wsub)
    assert_blocked(bash(layout, f"git -C {wsub} commit -m x", cwd=worktree, env=env), "G1")
    assert_blocked(edit(layout, wsub / "Cargo.toml", cwd=worktree, env=env), "G4")


# --------------------------------------------------------------------------- S-039


@pytest.mark.parametrize(
    "cmd",
    [
        "git status",
        "git commit -m x",
        "git -C {mirror} log",
        "cargo build",
        "task verify",
        "git log -1  # cd external/ocx && git push",
        "git commit -m \"$(cat <<'EOF'\nfix: x\n\ncd external/ocx && git push\nEOF\n)\"",
        "git commit -F - <<-EOF\n\tbody: git -C external/ocx push\n\tEOF",
    ],
)
def test_s039_normal_mirror_work_allowed(layout, cmd):
    assert_allowed(bash(layout, cmd.format(mirror=layout.mirror)))


def test_s039_write_mirror_source_allowed(layout):
    assert_allowed(edit(layout, layout.mirror / "src" / "lib.rs"))


# --------------------------------------------------------------------------- S-040


def _doc_lines_mentioning_submodule():
    lines = []
    for rel in (".claude/skills/update-ocx/SKILL.md", "README.md"):
        fenced = False
        for line in (REPO_ROOT / rel).read_text(encoding="utf-8").splitlines():
            if re.match(r"\s*```", line):
                fenced = not fenced and re.match(r"\s*```sh\s*$", line) is not None
                continue
            if fenced and "external/ocx" in line:
                lines.append((rel, line.strip()))
    return lines


def test_s040_documented_submodule_commands_allowed(layout):
    lines = _doc_lines_mentioning_submodule()
    assert lines, "no fenced sh line mentions external/ocx — extraction broke"
    failures = []
    for rel, line in lines:
        result = bash(layout, line)
        if (result.returncode, result.stderr) != (0, ""):
            failures.append(f"{rel}: {line!r} -> {result.returncode} {result.stderr.strip()}")
    assert not failures, "\n".join(failures)


def test_s040_documented_commands_never_cd_into_submodule():
    offenders = [f"{rel}: {line}" for rel, line in _doc_lines_mentioning_submodule()
                 if "cd external/ocx" in line]
    assert not offenders, "\n".join(offenders)


# --------------------------------------------------------------------------- S-041


@pytest.mark.parametrize("stdin", ["not json", "", "[1, 2]", '"str"', "null"])
def test_s041_fail_open_on_malformed_input(layout, stdin):
    result = run_hook(layout, stdin)
    assert result.returncode == 0
    assert result.stderr.startswith("ocx-mirror guard: internal error ("), result.stderr


def test_s041_fail_open_on_unbalanced_quotes(layout):
    result = bash(layout, "echo \"a ' && git status")
    assert result.returncode == 0
    assert result.stderr.startswith("ocx-mirror guard: internal error ("), result.stderr


def test_s041_missing_tool_input_allowed(layout):
    assert_allowed(run_hook(layout, {"tool_name": "Bash", "cwd": str(layout.mirror)}))


def test_s041_injected_exception_fails_open(layout):
    code = (
        f"import sys; sys.path.insert(0, {str(HOOK_DIR)!r})\n"
        "import pre_tool_use_guard as g\n"
        "def boom(payload): raise RuntimeError('injected')\n"
        "g.evaluate = boom\n"
        "g.main()\n"
    )
    payload = {"tool_name": "Bash", "tool_input": {"command": "git -C external/ocx log"},
               "cwd": str(layout.mirror)}
    result = subprocess.run(
        ["python3", "-B", "-c", code],
        input=json.dumps(payload),
        env={**os.environ, "CLAUDE_PROJECT_DIR": str(layout.mirror)},
        capture_output=True, text=True, encoding="utf-8", timeout=30,
    )
    assert result.returncode == 0
    assert "internal error" in result.stderr and "injected" in result.stderr


if __name__ == "__main__":
    sys.exit(pytest.main([__file__, "-q"]))
