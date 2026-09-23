#!/usr/bin/env python3
"""PreToolUse guard for the external/ocx submodule and the sibling ocx checkout.

Rules G1-G5 per .claude/artifacts/adr_bazel_crate_split.md § C9(c):

  G1  git commit|merge|pull|push inside the submodule while its HEAD is main/detached
  G2  any git inside the submodule without an absolute -C (bash cwd persists)
  G3  recursive `git submodule update` from the superproject (reverts the ocx checkout)
  G4  Write/Edit inside the submodule while its HEAD is main/detached
  G5  writes or non-read git inside the sibling <main checkout>/../ocx

Exit 2 + one stderr line blocks; exit 0 allows. This is a rail against slips,
not a security boundary (security-threat-model.md: the environment is trusted),
so every internal failure allows.

ponytail: shell parsing is shlex, not bash. Expanded: $HOME/~, $PWD/${PWD},
and the exact spellings `$(pwd)`, `$(git rev-parse --show-toplevel)` and their
backtick forms. Not parsed: `bash -c`/`eval` payloads, other `$( )` beyond
treating its words as a subshell segment, other variables (`-C $SUB` is taken
literally), wrappers other than env/rtk/timeout/`ocx run|exec … --`/`uv run
--`, `--git-dir`/`--work-tree` targets, and redirections writing into the
submodule or sibling. The cwd spellings ($PWD, `pwd`, show-toplevel) count as
an absolute -C only when they resolve outside the submodule. G1 reads the
submodule HEAD before the command runs, so any non-creating `switch`/`checkout`
earlier in the same command marks it protected for later history writes —
including a pathspec checkout spelled without `--` (`checkout file`). A
branch-creating switch lifts the pre-run check only while every connector up to
the write is `&&` (a newline right after `&&` continues it) and the created name
is readable and not main; any other connector (`;`, newline, `||`, `|`, `&` —
also the `&` of `2>&1`) assumes the switch may have failed. A quoted `<<X` inside an argument is misread as a
heredoc start (the lines up to `X` are skipped). G5 blocks
`branch`/`remote`/`tag`/`config`/`stash` in the sibling even in their read
forms; allowing them needs per-subcommand argument parsing. Upgrade path if
slips get through there: parse `bash -c` strings recursively.
"""

import json
import os
import re
import shlex
import subprocess
import sys

SUBMODULE = ("external", "ocx")
TOPLEVEL_EXPR = "$(git rev-parse --show-toplevel)"
TOPLEVEL_WORD, PWD_WORD = "\0toplevel\0", "\0pwd\0"  # paren-free stand-ins lexed as words
SUBSTITUTIONS = re.compile(
    r"\$\(git rev-parse --show-toplevel\)|`git rev-parse --show-toplevel`"
    r"|\$\(pwd\)|`pwd`|\$\{PWD\}|\$PWD(?![A-Za-z0-9_])")
HEAD_MOVES = {"switch", "checkout"}
MOVED, CREATED, CREATED_AFTER_MOVE = "moved", "created", "created-after-move"
BRANCH_CREATE = re.compile(r"-[cCbB]|--(create|force-create|orphan)(=|$)")
PROTECTED_BRANCH = "main"
COMPLIANT_C = f'git -C "{TOPLEVEL_EXPR}/external/ocx"'
HISTORY_WRITES = {"commit", "merge", "pull", "push"}
SIBLING_READS = {
    "log", "show", "diff", "status", "rev-parse", "ls-files", "ls-tree", "grep",
    "blame", "cat-file", "describe", "merge-base", "rev-list", "for-each-ref",
    "show-ref", "shortlog", "ls-remote", "name-rev", "cherry", "range-diff",
}
EDIT_TOOLS = {"Write": "file_path", "Edit": "file_path", "MultiEdit": "file_path",
              "NotebookEdit": "notebook_path"}
PREFIX_WORDS = {"env", "rtk", "command", "exec", "nohup", "time",
                "do", "then", "else", "if", "while", "until", "!", "{"}
ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=")
HEREDOC = re.compile(r"(?<!<)<<(-?)\s*(['\"]?)([A-Za-z_][\w.-]*)\2")
TIMEOUT_ARG_OPTS = {"-s", "-k", "--signal", "--kill-after"}
GIT_TIMEOUT = 5


# --------------------------------------------------------------------------- paths


def parts(path):
    return os.path.realpath(path).split(os.sep)


def is_inside(path, root):
    """Component-wise containment: /dev/ocx-mirror is not inside /dev/ocx."""
    p, r = parts(path), parts(root)
    return p[: len(r)] == r


def submodule_dir(path):
    """Nearest ancestor-or-self `<X>/external/ocx` whose `<X>/.gitmodules` names it."""
    p = parts(path)
    for end in range(len(p), 1, -1):
        if tuple(p[end - 2 : end]) != SUBMODULE:
            continue
        candidate = os.sep.join(p[:end])
        gitmodules = os.path.join(os.path.dirname(os.path.dirname(candidate)), ".gitmodules")
        if names_submodule(gitmodules):
            return candidate
    return None


def lexically_submodule(path):
    """True when `path` spells `…/external/ocx` or a path below one, existing or not."""
    p = os.path.normpath(path).split(os.sep)
    return any(tuple(p[i : i + 2]) == SUBMODULE for i in range(len(p) - 1))


def names_submodule(gitmodules):
    try:
        with open(gitmodules, encoding="utf-8") as fh:
            text = fh.read()
    except OSError:
        return False
    return re.search(r"^\s*path\s*=\s*external/ocx\s*$", text, re.MULTILINE) is not None


def git_toplevel(path):
    """Nearest ancestor-or-self holding a `.git` entry (file or dir), or None."""
    current = os.path.realpath(path)
    while True:
        if os.path.exists(os.path.join(current, ".git")):
            return current
        parent = os.path.dirname(current)
        if parent == current:
            return None
        current = parent


def main_checkout(start):
    """Parent of the git common dir — resolves linked worktrees to the main checkout."""
    top = git_toplevel(start)
    if top is None:
        return None
    dotgit = os.path.join(top, ".git")
    if os.path.isdir(dotgit):
        return top
    with open(dotgit, encoding="utf-8") as fh:
        gitdir = fh.read().strip().removeprefix("gitdir:").strip()
    gitdir = os.path.join(top, gitdir)
    commondir = os.path.join(gitdir, "commondir")
    if os.path.exists(commondir):
        with open(commondir, encoding="utf-8") as fh:
            gitdir = os.path.join(gitdir, fh.read().strip())
    return os.path.dirname(os.path.realpath(gitdir))


def sibling_dir(project):
    checkout = main_checkout(project)
    return None if checkout is None else os.path.join(os.path.dirname(checkout), "ocx")


def in_sibling(path, project):
    if "ocx" not in parts(path):  # cheap pre-filter: the sibling is always named ocx
        return False
    sibling = sibling_dir(project)
    return sibling is not None and is_inside(path, sibling)


def git(directory, *args):
    return subprocess.run(
        ["git", "-C", directory, *args],
        capture_output=True, text=True, encoding="utf-8", timeout=GIT_TIMEOUT,
        stdin=subprocess.DEVNULL,
    )


def protected_head(sub):
    """True when the submodule HEAD is `main` or detached."""
    result = git(sub, "symbolic-ref", "-q", "--short", "HEAD")
    return result.returncode != 0 or result.stdout.strip() == PROTECTED_BRANCH


def expand(word, base):
    """Resolve a cd/-C argument against `base`; returns (path, was_absolute)."""
    if word == TOPLEVEL_WORD or word.startswith(TOPLEVEL_WORD + "/"):
        result = git(base, "rev-parse", "--show-toplevel")
        top = result.stdout.strip() if result.returncode == 0 else base
        return top + word[len(TOPLEVEL_WORD):], submodule_dir(top) is None
    if word == PWD_WORD or word.startswith(PWD_WORD + "/"):
        # a cwd spelling is only as absolute as the cwd: inside the submodule it is a G2 slip
        return base + word[len(PWD_WORD):], submodule_dir(base) is None
    for var in ("${HOME}", "$HOME"):
        if word == var or word.startswith(var + "/"):
            word = "~" + word[len(var):]
    word = os.path.expanduser(word)
    return os.path.join(base, word), os.path.isabs(word)


# --------------------------------------------------------------------------- shell


def strip_heredocs(text):
    """Drop heredoc bodies — their text is data, not commands."""
    out, pending = [], []
    for line in text.split("\n"):
        if pending:
            dash, word = pending[0]
            if (line.lstrip("\t") if dash else line) == word:
                pending.pop(0)
            continue
        out.append(line)
        pending = [(m.group(1) == "-", m.group(3)) for m in HEREDOC.finditer(line)]
    return "\n".join(out)


def strip_comments(text):
    """Drop `# …` to end of line when `#` starts a word outside quotes."""
    out, quote, i = [], None, 0
    while i < len(text):
        c = text[i]
        if quote:
            if c == quote:
                quote = None
            elif c == "\\" and quote == '"' and i + 1 < len(text):
                out.append(c)
                i += 1
                c = text[i]
        elif c in "'\"":
            quote = c
        elif c == "\\" and i + 1 < len(text):
            out.append(c)
            i += 1
            c = text[i]
        elif c == "#" and (not out or out[-1] in " \t\n;&|()"):
            while i < len(text) and text[i] != "\n":
                i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def segments(command):
    """Split a command line into simple-command word lists on ; && || | & and newline.

    Parentheses are yielded as the strings "(" and ")" so callers can scope `cd`
    to a subshell or command substitution; each separator is yielded as "&&"
    or, for any other connector, ";".
    """
    text = strip_comments(strip_heredocs(command)).replace("\\\n", " ").rstrip("\\")
    text = SUBSTITUTIONS.sub(lambda m: TOPLEVEL_WORD if "rev-parse" in m[0] else PWD_WORD, text)
    lexer = shlex.shlex(text, posix=True, punctuation_chars="();<>|&\n")
    lexer.whitespace = " \t\r"
    lexer.whitespace_split = True
    lexer.commenters = ""
    current, after_connector = [], False
    for token in lexer:
        if token and all(ch in "();<>|&\n" for ch in token):
            if any(ch in "();|&\n" for ch in token) and current:
                yield current
                current = []
            # a newline right after a connector continues it (`&&⏎ git …` is still `&&`)
            separator = "".join(ch for ch in token if ch in ";|&")
            if not separator and "\n" in token and not after_connector:
                separator = "\n"
            if separator:
                yield "&&" if separator == "&&" else ";"
                after_connector = True
            elif token.strip("\n"):
                after_connector = False
            yield from (ch for ch in token if ch in "()")
            continue  # redirection operators are dropped
        current.append(token)
        after_connector = False
    if current:
        yield current


def command_words(words):
    """Skip env assignments, wrappers and shell keywords in front of the command."""
    i = 0
    while i < len(words):
        word = words[i]
        if ASSIGNMENT.match(word) or word in PREFIX_WORDS:
            i += 1
        elif word == "proxy" and i > 0 and words[i - 1] == "rtk":
            i += 1
        elif word.startswith("-") and i > 0 and words[i - 1] == "env":
            i += 1
        elif (word in ("ocx", "uv") and i + 1 < len(words)
              and words[i + 1] in ("run", "exec") and "--" in words[i + 2 :]):
            i = words.index("--", i + 2) + 1
        elif word == "timeout":
            i += 1
            while i < len(words) and words[i].startswith("-"):
                i += 2 if words[i] in TIMEOUT_ARG_OPTS else 1
            i += 1  # the duration
        else:
            break
    return words[i:]


def parse_git(args, base):
    """Return (effective dir, has absolute -C, subcommand, rest) for `git <args>`."""
    directory, absolute, i = base, False, 0
    while i < len(args):
        arg = args[i]
        if arg == "-C" and i + 1 < len(args):
            directory, was_abs = expand(args[i + 1], directory)
            absolute = absolute or was_abs
            i += 2
        elif arg == "-c" and i + 1 < len(args):
            i += 2
        elif arg.startswith("-"):
            i += 1
        else:
            return directory, absolute, arg, args[i + 1 :]
    return directory, absolute, None, []


def created_branch(rest):
    """Name a branch-creating switch/checkout creates, or None when unknowable."""
    for i, word in enumerate(rest):
        match = BRANCH_CREATE.match(word)
        if match is None:
            continue
        if match[2] == "=":  # --create=NAME
            name = word[match.end():]
        elif match[1] is None and len(word) > 2:  # -cNAME
            name = word[2:]
        else:
            name = rest[i + 1] if i + 1 < len(rest) else ""
        return None if not name or name.startswith("-") or re.search(r"[$`\0]", name) else name
    return None


def check_git(args, base, project, heads, session_cwd):
    """Verdict for one git invocation; `heads` maps a submodule whose HEAD an
    earlier segment of the same command switched to MOVED (HEAD unknowable) or
    to CREATED/CREATED_AFTER_MOVE (a new branch, while the `&&` chain holds)."""
    directory, absolute, sub_cmd, rest = parse_git(args, base)
    sub = submodule_dir(directory)
    if sub is not None:
        if sub_cmd in HISTORY_WRITES and heads.get(sub) == MOVED:
            return (f"G1: git {sub_cmd} in {sub} after a switch/checkout earlier in this "
                    f"command (its HEAD is unknowable before it runs) — use {COMPLIANT_C} "
                    "switch … as its own command first")
        if sub_cmd in HISTORY_WRITES and sub not in heads and protected_head(sub):
            return (f"G1: git {sub_cmd} in {sub} on main or detached HEAD — use "
                    f"{COMPLIANT_C} switch -c <branch> first")
        if not absolute:
            cd_first = (f"first cd {shlex.quote(project)} (the session cwd is inside the "
                        "submodule), then " if submodule_dir(session_cwd) else "")
            return (f"G2: git inside {sub} without an absolute -C (the bash cwd persists) "
                    f"— use {cd_first}{COMPLIANT_C} …")
        pathspec_only = (sub_cmd == "checkout" and "--" in rest
                         and rest[rest.index("--") + 1:])  # `checkout [<tree>] -- <paths>`
        renames_to_main = (sub_cmd == "branch" and PROTECTED_BRANCH in rest
                           and any(w in ("-m", "-M", "--move") for w in rest))
        if renames_to_main:  # `branch -M main` moves HEAD onto main with no switch
            heads[sub] = MOVED
        elif sub_cmd in HEAD_MOVES and not pathspec_only:
            if created_branch(rest) in (None, PROTECTED_BRANCH):  # (re)creating main is a move
                heads[sub] = MOVED
            elif heads.get(sub) in (MOVED, CREATED_AFTER_MOVE):
                heads[sub] = CREATED_AFTER_MOVE
            else:
                heads[sub] = CREATED
    elif (sub_cmd == "submodule" and "--recursive" in rest
          and next((w for w in rest if not w.startswith("-")), None) == "update"
          and not lexically_submodule(directory)):
        top = git_toplevel(directory)
        if top is not None and names_submodule(os.path.join(top, ".gitmodules")):
            return ("G3: recursive submodule update from the superproject reverts the "
                    f"ocx checkout — use {COMPLIANT_C} submodule update --init --recursive")
    if sub_cmd and sub_cmd not in SIBLING_READS and in_sibling(directory, project):
        return (f"G5: git {sub_cmd} in the sibling ocx checkout {os.path.realpath(directory)} "
                "— use /ocx-upstream-pr (work in external/ocx on a feature branch)")
    return None


def check_bash(command, cwd, project):
    if "git" not in command:  # cheap exit for the common case
        return None
    base, saved, heads = cwd, [], {}
    for words in segments(command):
        if words == "&&":
            continue
        if words == ";":  # the switch may have failed: fall back to the state before it
            for sub, head in list(heads.items()):
                if head == CREATED:
                    del heads[sub]
                elif head == CREATED_AFTER_MOVE:
                    heads[sub] = MOVED
            continue
        if words == "(":
            saved.append(base)
            continue
        if words == ")":
            base = saved.pop() if saved else base  # `case a)` closes nothing
            continue
        words = command_words(words)
        if not words:
            continue
        name, args = words[0], words[1:]
        if name in ("cd", "pushd"):
            targets = [a for a in args if not a.startswith("-")]
            base = expand(targets[0], base)[0] if targets else os.path.expanduser("~")
        elif name == "git" or name.endswith("/git"):
            verdict = check_git(args, base, project, heads, cwd)
            if verdict:
                return verdict
    return None


def check_edit(path, cwd, project):
    path = os.path.join(cwd, path)
    sub = submodule_dir(path)
    if sub is not None and protected_head(sub):
        return (f"G4: edit inside {sub} on main or detached HEAD — use "
                f"{COMPLIANT_C} switch -c <branch> first")
    if in_sibling(path, project):
        return (f"G5: edit in the sibling ocx checkout {os.path.realpath(path)} — use "
                "/ocx-upstream-pr (work in external/ocx on a feature branch)")
    return None


# --------------------------------------------------------------------------- entry


def evaluate(payload):
    """Return a block message (without the `ocx-mirror guard ` prefix) or None."""
    if not isinstance(payload, dict):
        raise ValueError(f"payload is {type(payload).__name__}, not an object")
    tool = payload.get("tool_name")
    tool_input = payload.get("tool_input") or {}
    cwd = payload.get("cwd") or os.getcwd()
    project = os.environ.get("CLAUDE_PROJECT_DIR") or cwd
    if tool == "Bash":
        return check_bash(tool_input.get("command") or "", cwd, project)
    if tool in EDIT_TOOLS and tool_input.get(EDIT_TOOLS[tool]):
        return check_edit(tool_input[EDIT_TOOLS[tool]], cwd, project)
    return None


def main():
    try:
        verdict = evaluate(json.loads(sys.stdin.read()))
    except Exception as exc:  # fail open: a guard bug must never stop the session
        print(f"ocx-mirror guard: internal error ({exc!r}), allowing", file=sys.stderr)
        sys.exit(0)
    if verdict:
        print(f"ocx-mirror guard {verdict}", file=sys.stderr)
        sys.exit(2)
    sys.exit(0)


if __name__ == "__main__":
    main()
