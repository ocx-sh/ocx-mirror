// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Where a spec sits in the repository, and what that implies for the file
//! names it renders.
//!
//! A repository may hold several specs. The root `mirror.yml` renders the
//! unsuffixed workflow names the goldens are pinned to; every other spec gets
//! a suffix derived from its path, so two specs never collide on one output.

use std::path::{Path, PathBuf};

use crate::error::MirrorError;

/// What `--spec` defaults to, and therefore the one spec path a generated
/// invocation can leave unsaid.
pub const DEFAULT_SPEC_PATH: &str = "./mirror.yml";

/// The repo-root-relative spec path that `DEFAULT_SPEC_PATH` names.
pub const DEFAULT_SPEC_NAME: &str = "mirror.yml";

/// Where one spec's generated workflows land, and how they refer back to it.
///
/// Everything is derived from the spec's path *relative to the repository
/// root* — never from its position in the argument list, so passing the same
/// specs in a different order renders the same bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecSlot {
    /// Spec path relative to the repo root: `mirror.yml`, `py3.13/mirror.yml`.
    pub relative: PathBuf,
    /// This spec's whole `extends:` chain, repo-root-relative, nearest base
    /// first. Editing any of them changes what this spec renders and publishes,
    /// so all of them belong in its `paths:` trigger.
    pub extends: Vec<PathBuf>,
}

impl SpecSlot {
    pub fn new(spec: &Path, extends: &[PathBuf], repo_root: &Path) -> Result<Self, MirrorError> {
        let relative = spec.strip_prefix(repo_root).map_err(|_| {
            MirrorError::SpecUsageError(format!(
                "spec {} is not under the repository root {} — pass --repo-root",
                spec.display(),
                repo_root.display(),
            ))
        })?;
        // A base above the root is the same failure as a spec above the root,
        // one step further out: `paths:` can only name files the workflow's own
        // repository contains, so a trigger for it would silently never fire —
        // which is exactly what putting the chain in the trigger is fixing.
        let extends = extends
            .iter()
            .map(|base| {
                base.strip_prefix(repo_root).map(Path::to_path_buf).map_err(|_| {
                    MirrorError::SpecUsageError(format!(
                        "spec {} extends {}, which is not under the repository root {} — \
                         a `paths:` trigger cannot name a file outside the repository, so editing \
                         that base would run nothing; move it under the root or pass --repo-root",
                        spec.display(),
                        base.display(),
                        repo_root.display(),
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            relative: relative.to_path_buf(),
            extends,
        })
    }

    /// The `extends` chain as `paths:` entries, dropping any base the spec's own
    /// subtree glob already covers.
    pub fn extends_entries(&self) -> Vec<String> {
        let own_subtree = self.dir().map(|dir| format!("{}/", slash_path(dir)));
        self.extends
            .iter()
            .map(|base| slash_path(base))
            .filter(|base| !own_subtree.as_ref().is_some_and(|prefix| base.starts_with(prefix)))
            .collect()
    }

    /// The spec's own directory relative to the repo root; `None` at the root.
    pub fn dir(&self) -> Option<&Path> {
        self.relative.parent().filter(|dir| !dir.as_os_str().is_empty())
    }

    /// Filename suffix for this spec's workflows — empty at the repo root,
    /// `-py3.13` for `py3.13/mirror.yml`, `-a-b` for `a/b/mirror.yml`.
    pub fn suffix(&self) -> String {
        match self.dir() {
            None => String::new(),
            Some(dir) => format!("-{}", slash_path(dir).replace('/', "-")),
        }
    }

    /// Repo-root-relative path of one of this spec's generated workflows.
    pub fn workflow(&self, stem: &str) -> PathBuf {
        PathBuf::from(format!(".github/workflows/{}", self.workflow_name(stem)))
    }

    /// Bare filename of one of this spec's generated workflows.
    pub fn workflow_name(&self, stem: &str) -> String {
        format!("{stem}{}.yml", self.suffix())
    }

    /// The spec path as the generated workflows spell it: repo-root-relative
    /// and `/`-separated, because they are read on a Linux runner.
    pub fn source(&self) -> String {
        slash_path(&self.relative)
    }

    /// `--spec <path>` threaded into this spec's generated invocations.
    ///
    /// Empty for the repo-root `mirror.yml`, whose path is exactly what every
    /// pipeline command already defaults to — omitting it is what keeps the
    /// published mirror repositories byte-identical to what they have
    /// committed. Any other location is named explicitly.
    pub fn spec_arg(&self) -> String {
        if self.is_default() {
            String::new()
        } else {
            format!(" --spec {}", self.source())
        }
    }

    pub fn is_default(&self) -> bool {
        self.relative == Path::new(DEFAULT_SPEC_NAME)
    }
}

/// `Path` → `/`-separated string. Generated workflows run on a Linux runner,
/// so the separator is never the generating host's.
pub fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The nearest ancestor of `dir` holding a `.git`, or `None` outside a
/// repository.
///
/// `.git` is a directory in a normal clone and a *file* in a worktree or
/// submodule checkout, so existence is the test, not file type.
///
/// ponytail: sync `exists()` in an async fn — one short upward walk, once per
/// invocation. Reach for `tokio::fs` here only if this ever runs per-spec.
pub fn git_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Resolve a path against the filesystem so spec paths and the repository root
/// are comparable regardless of which of them the caller spelled relatively.
pub async fn canonical(path: &Path) -> Result<PathBuf, MirrorError> {
    tokio::fs::canonicalize(path)
        .await
        .map_err(|e| MirrorError::SpecUsageError(format!("cannot resolve {}: {e}", path.display())))
}

/// The directory holding `spec`, with a bare filename resolving to `.`.
pub fn spec_parent(spec: &Path) -> &Path {
    match spec.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Deepest directory both paths lie under. Both are canonical, so they share at
/// least the filesystem root.
pub fn common_ancestor(a: &Path, b: &Path) -> PathBuf {
    a.components()
        .zip(b.components())
        .take_while(|(x, y)| x == y)
        .map(|(x, _)| x)
        .collect()
}

/// `"<spec>: "` when the run covers several specs, so a warning names the one
/// it is about; empty for the single-spec repository, where it would be noise.
pub fn label(slot: &SpecSlot, named: bool) -> String {
    if named {
        format!("{}: ", slot.source())
    } else {
        String::new()
    }
}

/// Render `on.*.paths` entries for one spec.
///
/// A repo-root spec keeps the repo-wide list it has always had. A spec in a
/// subdirectory watches that subtree instead, so editing `py3.13/` never wakes
/// the sibling specs' workflows. `root_entries` is the root spec's list minus
/// its own workflow file, which is appended for both cases.
///
/// Every base in the spec's `extends:` chain is watched too: the shared base of
/// a multi-spec repository sits *outside* every child's subtree, so a change to
/// the platform matrix or the container set there would otherwise re-run
/// nothing at all.
pub fn trigger_paths(slot: &SpecSlot, root_entries: &[String], stem: &str) -> String {
    let mut entries = match slot.dir() {
        None => root_entries.to_vec(),
        Some(dir) => vec![format!("{}/**", slash_path(dir))],
    };
    entries.extend(slot.extends_entries());
    entries.push(format!(".github/workflows/{}", slot.workflow_name(stem)));

    // A nested spec's subtree trigger only covers files under that subtree,
    // while a test's `script:` path resolves from the repository root — unlike
    // `metadata.default` and `catalog.*`, which resolve against the spec's own
    // directory. Say so where the gap is, or the first Starlark test in a
    // subdirectory spec silently stops triggering its own workflow.
    let note = match slot.dir() {
        None => String::new(),
        Some(dir) => format!(
            "      # `script:` paths resolve from the repository root, not from {0}/ —\n\
             \x20     # keep this spec's scripts under {0}/ so editing one triggers this run.\n",
            slash_path(dir),
        ),
    };

    format!("{note}{}", indent_entries(&entries))
}

/// Format path entries as a YAML sequence under `paths:` (no trailing newline —
/// the templates supply the surrounding lines).
pub fn indent_entries(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| format!("      - {entry}"))
        .collect::<Vec<_>>()
        .join("\n")
}
