# Verification Blocks

The commands behind every `see block ID` cell in `rust-cargo.md`, grouped by the table the cell sits in. Run a block from the repository root. File lists come from `git ls-files`, so no command needs ripgrep or a GNU-only grep option, and a pathspec that matches no file contributes nothing. Empty output passes unless the row says otherwise. `CHECKS` is where this rule's `checks/` directory is installed.

## Lint Policy

```sh
CHECKS=.claude/rules/rust-cargo/checks
# LINT-01
git ls-files -z --cached --others --exclude-standard -- ':(glob)**/Cargo.toml' ':(exclude,glob)external/**' | xargs -0 -r grep -l -E -e '^\[lints\]' --
# LINT-02
git ls-files -z --cached --others --exclude-standard -- ':(glob).github/**' ':(glob).cargo/**' ':(glob)taskfiles/**' | xargs -0 -r grep -H -n -E -e 'RUSTFLAGS' --
# LINT-07
git ls-files -z --cached --others --exclude-standard -- ':(glob)**/Cargo.toml' ':(exclude,glob)external/**' | xargs -0 -r grep -H -n -E -e 'unsafe_code' --
# LINT-08
git ls-files -z --cached --others --exclude-standard -- ':(glob)**/*.rs' ':(exclude,glob)external/**' | PAT='#\[expect\([^]]*\]' NOT='reason' xargs -0 -r awk -f "$CHECKS/multiline.awk" --
# LINT-17
git ls-files -z --cached --others --exclude-standard -- ':(glob)**/*.rs' ':(exclude,glob)external/**' | xargs -0 -r grep -o -E -e '#\[(allow|expect)\(clippy::' -- | wc -l
```

## Toolchain and Tooling

```sh
# TOOL-04
git ls-files -z --cached --others --exclude-standard -- ':(glob).github/**' ':(glob)taskfiles/**' | xargs -0 -r grep -H -n -E -e '\+nightly' -e '[-]Z ' --
# TOOL-04 (2)
git ls-files -z --cached --others --exclude-standard -- ':(glob)**/*.rs' ':(exclude,glob)external/**' | xargs -0 -r grep -H -n -E -e '#!\[feature\(' --
```

## CI and Supply-Chain Gates

```sh
# CI-01
git ls-files -z --cached --others --exclude-standard -- ':(glob).github/workflows/**/*.yml' | xargs -0 -r grep -L -E -e '^permissions: \{\}' --
# CI-02
git ls-files -z --cached --others --exclude-standard -- ':(glob).github/workflows/**' | xargs -0 -r grep -H -n -E -e 'uses: .*@' -- | grep -v -E -e '^[^:]+:[0-9]+:.*uses: .*@[0-9a-f]{40}'
# CI-04
git ls-files -z --cached --others --exclude-standard -- ':(glob).github/**' ':(glob)taskfiles/**' | xargs -0 -r grep -H -n -E -e '^[[:space:]]*(- )?(run: )?cargo [bctn][a-z]+' -- | grep -v -E -e '^[^:]+:[0-9]+:.*--locked'
```

## Release and Distribution

```sh
# REL-02
git ls-files -z --cached --others --exclude-standard -- ':(glob)**/*.rs' ':(exclude,glob)external/**' | xargs -0 -r grep -H -n -E -e 'catch_unwind' -e 'resume_unwind' --
# REL-06
git ls-files -z --cached --others --exclude-standard -- ':(glob).github/workflows/**' | xargs -0 -r grep -H -n -E -e 'CARGO_REGISTRY_TOKEN' -e '_TOKEN: \$\{\{ secrets' --
```
