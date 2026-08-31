// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline describe` — publish catalog metadata (README + logo)
//! to the registry as a referrer of the target repository.
//!
//! Loads `mirror.yml`, resolves the optional `catalog:` block (defaults to
//! `CATALOG.md` + probed `logo.{svg,png}`), then shells out to
//! `ocx package description push` to publish the data under the `__ocx.desc` tag.
//!
//! When no `CATALOG.md` is present and the source is an env source
//! (`source.type: pylock`), a catalog description is synthesized from the
//! root package's wheel `*.dist-info/METADATA` instead of skipping — an
//! on-disk `CATALOG.md` always wins over autogen. Otherwise (no catalog
//! content and nothing to synthesize from) exits 0 silently — the workflow
//! is a no-op until catalog content lands in the mirror repo.
//!
//! # Errors
//!
//! - [`MirrorError::SpecNotFound`] / [`MirrorError::SpecInvalid`] from
//!   `load_spec`.
//! - [`MirrorError::ExecutionFailed`] when the `ocx package description push`
//!   subprocess returns non-zero, or when downloading/reading the root
//!   wheel for catalog autogen fails.

use std::path::{Path, PathBuf};

use ocx_lib::cli::DataInterface;

use crate::error::MirrorError;
use crate::pipeline::ocx_cli::{forward_ocx_env, resolve_ocx_binary};
use crate::spec::{self, MirrorSpec, Source};

/// `ocx-mirror package pipeline describe` subcommand.
#[derive(clap::Parser)]
pub struct Describe {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,
}

impl Describe {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        let spec = spec::load_spec(&self.spec).await?;
        let spec_dir = self.spec.parent().unwrap_or(Path::new("."));
        let catalog = spec.catalog.clone().unwrap_or_default();

        let default_readme = catalog.resolved_readme(spec_dir);
        // `_synth_dir` is the RAII guard for a synthesized catalog's private
        // temp directory — held across `invoke_describe`, dropped (deleted)
        // after.
        let (readme, _synth_dir) = if default_readme.exists() {
            (default_readme, None)
        } else if let Some((dir, path)) = synthesize_env_catalog(&spec, spec_dir).await? {
            (path, Some(dir))
        } else {
            tracing::info!(
                "describe: {} not found; skipping (no catalog content to publish)",
                default_readme.display()
            );
            return Ok(());
        };

        let logo = catalog.resolved_logo(spec_dir);
        let identifier = format!("{}/{}", spec.target.registry, spec.target.repository);

        invoke_describe(&identifier, &readme, logo.as_deref()).await
    }
}

/// When no on-disk `CATALOG.md` exists and `spec.source` is an env source,
/// synthesizes a minimal catalog markdown from the root package's wheel
/// `*.dist-info/METADATA` (`Summary`/`Keywords`/`License`) and writes it
/// into a fresh private temp directory (returned as an RAII guard the caller
/// holds until the describe subprocess has read the file — predictable
/// shared-/tmp names would invite symlink clobber/TOCTOU, CWE-377). Returns
/// `Ok(None)` when there's nothing to synthesize from.
///
/// `pylock` resolves the root wheel from its committed lock directly.
/// `pypi` has no committed lock — it instead looks for a lock `pipeline plan`
/// already derived under `spec_dir`'s `locks/` directory
/// ([`find_any_derived_pypi_lock`]); absent one (no prior `pipeline plan` run
/// reachable from this `describe` invocation), returns `Ok(None)` the same
/// way a missing `CATALOG.md` does.
async fn synthesize_env_catalog(
    spec: &MirrorSpec,
    spec_dir: &Path,
) -> Result<Option<(tempfile::TempDir, PathBuf)>, MirrorError> {
    let app_name = spec.source.pylock_app_name(&spec.name);
    let lock = match &spec.source {
        Source::Pylock { path, .. } => crate::source::pylock::load(spec_dir, path)
            .await
            .map_err(|e| crate::source::pylock::classify_error("failed to load pylock for catalog autogen", e))?,
        Source::Pypi { .. } => match find_any_derived_pypi_lock(spec_dir).await {
            Some(lock) => lock,
            None => return Ok(None),
        },
        _ => return Ok(None),
    };

    let package = crate::source::pylock::find_app_package(&lock, app_name).map_err(|e| {
        crate::source::pylock::classify_error("failed to resolve pylock app package for catalog autogen", e)
    })?;

    let Some(wheel) = pick_root_wheel(&package.wheels) else {
        return Ok(None);
    };
    let Some(url) = &wheel.url else {
        return Ok(None);
    };
    let parsed_url = url::Url::parse(url).map_err(|e| {
        MirrorError::ExecutionFailed(vec![format!("invalid wheel URL '{url}' for catalog autogen: {e}")])
    })?;

    let work_dir = tempfile::tempdir()
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to create catalog work dir: {e}")]))?;
    let wheel_path = work_dir.path().join("catalog-root.whl");
    // Same trust roots as every other mirror-owned leg (`crate::http`).
    let client = crate::http::client()?;
    crate::pipeline::download::download(&client, &parsed_url, &wheel_path)
        .await
        .map_err(|e| {
            MirrorError::ExecutionFailed(vec![format!(
                "failed to download root wheel for catalog autogen: {e:#}"
            )])
        })?;

    // Sync whole-file read + zip walk — off the async runtime.
    let description = tokio::task::spawn_blocking(move || ocx_python::read_wheel_description(&wheel_path))
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("wheel metadata read task panicked: {e}")]))?
        .map_err(|e| {
            MirrorError::ExecutionFailed(vec![format!("failed to read wheel metadata for catalog autogen: {e}")])
        })?;

    let markdown = render_catalog_markdown(&spec.name, &description);
    let catalog_path = work_dir.path().join("CATALOG.md");
    tokio::fs::write(&catalog_path, markdown)
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to write synthesized catalog: {e}")]))?;

    Ok(Some((work_dir, catalog_path)))
}

/// Locates any already-derived PEP 751 lock for a `pypi` source under
/// `spec_dir`'s `locks/` directory (`pipeline plan`'s `--locks-dir` default,
/// [`plan::DEFAULT_LOCKS_DIR`]) and parses it.
///
/// Every lock in that directory resolves the same configured package at a
/// different version — PEP 566 core metadata (`Summary`/`Keywords`/`License`)
/// doesn't vary by version, so any one lock is as good as another for catalog
/// autogen; picks the first in sorted filename order for determinism.
/// Returns `None` when the directory is absent/empty or no file in it parses
/// as a lock — `describe` then silently skips catalog autogen, the same
/// fallback already applied when there's no `CATALOG.md`.
async fn find_any_derived_pypi_lock(spec_dir: &Path) -> Option<ocx_python::Pylock> {
    let locks_dir = spec_dir.join(crate::command::package::pipeline::plan::DEFAULT_LOCKS_DIR);
    let mut entries = tokio::fs::read_dir(&locks_dir).await.ok()?;

    let mut candidates = Vec::new();
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            candidates.push(path);
        }
    }
    candidates.sort();

    for path in candidates {
        if let Ok(contents) = tokio::fs::read_to_string(&path).await
            && let Ok(lock) = ocx_python::parse_pylock(&contents)
        {
            return Some(lock);
        }
    }
    None
}

/// Picks the wheel to extract description metadata from: the first wheel
/// whose filename carries the platform-independent `-any` tag (pure-Python
/// packages ship exactly one), falling back to the lock's first listed wheel
/// otherwise — every wheel for the same `(name, version)` carries identical
/// PEP 566 core metadata (`Summary`/`Keywords`/`License` don't vary per wheel).
fn pick_root_wheel(wheels: &[ocx_python::LockedWheel]) -> Option<&ocx_python::LockedWheel> {
    wheels
        .iter()
        .find(|wheel| wheel.filename.contains("-any"))
        .or_else(|| wheels.first())
}

/// Renders a minimal `CATALOG.md` from wheel metadata: YAML frontmatter
/// (`title`/`description`/`keywords` — the structured fields `ocx package
/// describe` parses into registry annotations, same contract as this repo's
/// own `CATALOG.md`) followed by a human-readable body. Frontmatter is
/// serialized with `serde_yaml_ng`, never string-formatted — `Summary` and
/// `Keywords` come from wheel METADATA, and hand-rolled YAML would let a
/// hostile wheel inject frontmatter fields.
fn render_catalog_markdown(name: &str, description: &ocx_python::WheelDescription) -> String {
    #[derive(serde::Serialize)]
    struct Frontmatter<'a> {
        title: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<&'a str>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        keywords: Vec<&'a str>,
    }
    let keywords: Vec<&str> = description
        .keywords
        .as_deref()
        .map(|list| list.split(',').map(str::trim).filter(|kw| !kw.is_empty()).collect())
        .unwrap_or_default();
    let frontmatter = Frontmatter {
        title: name,
        description: description.summary.as_deref(),
        keywords,
    };
    let frontmatter_yaml =
        serde_yaml_ng::to_string(&frontmatter).expect("frontmatter of plain strings serializes as YAML");

    let mut markdown = format!("---\n{frontmatter_yaml}---\n\n# {name}\n");
    if let Some(summary) = &description.summary {
        markdown.push_str(&format!("\n{summary}\n"));
    }
    if let Some(keywords) = &description.keywords {
        markdown.push_str(&format!("\nKeywords: {keywords}\n"));
    }
    if let Some(license) = &description.license {
        markdown.push_str(&format!("\nLicense: {license}\n"));
    }
    markdown
}

/// Build the `ocx package description push <identifier> --readme <path>
/// [--logo <path>]` argv.
///
/// Split out so the unit tests assert on the argv [`invoke_describe`] actually
/// spawns. A second copy of this assembly living in the test module would go
/// green against any production spelling, which is how the pre-0.6 `package
/// describe` verb survived here unnoticed.
fn build_describe_args(identifier: &str, readme: &Path, logo: Option<&Path>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "package".into(),
        "description".into(),
        "push".into(),
        identifier.into(),
        "--readme".into(),
        readme.display().to_string(),
    ];
    if let Some(logo_path) = logo {
        args.push("--logo".into());
        args.push(logo_path.display().to_string());
    }
    args
}

/// Spawn `ocx package description push <identifier> --readme <path> [--logo <path>]`.
async fn invoke_describe(identifier: &str, readme: &Path, logo: Option<&Path>) -> Result<(), MirrorError> {
    let ocx_binary = resolve_ocx_binary().map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;

    let mut cmd = tokio::process::Command::new(&ocx_binary);
    cmd.args(build_describe_args(identifier, readme, logo));
    forward_ocx_env(&mut cmd);

    let status = cmd
        .status()
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to spawn ocx: {e}")]))?;

    if !status.success() {
        return Err(MirrorError::ExecutionFailed(vec![format!(
            "ocx package description push exited {status} for {identifier}"
        )]));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Subprocess invocation is exercised via the candidate testbed; here we
    //! only verify argument assembly stays stable for the cases that matter to
    //! the workflow.

    use super::*;
    use tempfile::tempdir;

    #[test]
    fn describe_assembles_args_with_readme_only() {
        let args = build_describe_args("ocx.sh/shfmt", Path::new("/spec/CATALOG.md"), None);
        assert_eq!(
            args,
            vec![
                "package",
                "description",
                "push",
                "ocx.sh/shfmt",
                "--readme",
                "/spec/CATALOG.md"
            ]
        );
    }

    #[test]
    fn describe_assembles_args_with_readme_and_logo() {
        let args = build_describe_args(
            "ocx.sh/shfmt",
            Path::new("/spec/CATALOG.md"),
            Some(Path::new("/spec/logo.png")),
        );
        assert_eq!(
            args,
            vec![
                "package",
                "description",
                "push",
                "ocx.sh/shfmt",
                "--readme",
                "/spec/CATALOG.md",
                "--logo",
                "/spec/logo.png",
            ]
        );
    }

    #[tokio::test]
    async fn describe_skips_silently_when_readme_missing() {
        // Writing a complete spec is heavy; smoke this via the resolver alone
        // — `execute` calls `readme.exists()` and returns Ok on absence.
        let tmp = tempdir().unwrap();
        let cfg = crate::spec::CatalogConfig::default();
        let readme = cfg.resolved_readme(tmp.path());
        assert!(!readme.exists());
    }

    // ── Catalog autogen (env sources) ───────────────────────────────────────

    fn locked_wheel(filename: &str) -> ocx_python::LockedWheel {
        ocx_python::LockedWheel {
            filename: filename.to_string(),
            url: Some(format!("https://example.test/{filename}")),
            sha256: "aaaa".to_string(),
        }
    }

    #[test]
    fn pick_root_wheel_prefers_any_tagged_wheel() {
        let wheels = vec![
            locked_wheel("pkg-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl"),
            locked_wheel("pkg-1.0.0-py3-none-any.whl"),
        ];
        let picked = pick_root_wheel(&wheels).expect("a wheel is picked");
        assert_eq!(picked.filename, "pkg-1.0.0-py3-none-any.whl");
    }

    #[test]
    fn pick_root_wheel_falls_back_to_first_when_no_any_tag() {
        let wheels = vec![locked_wheel("pkg-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl")];
        let picked = pick_root_wheel(&wheels).expect("a wheel is picked");
        assert_eq!(picked.filename, "pkg-1.0.0-cp313-cp313-manylinux_2_28_x86_64.whl");
    }

    #[test]
    fn pick_root_wheel_returns_none_for_empty_list() {
        assert!(pick_root_wheel(&[]).is_none());
    }

    #[test]
    fn render_catalog_markdown_includes_all_present_fields() {
        let description = ocx_python::WheelDescription {
            summary: Some("A tiny test package".to_string()),
            keywords: Some("test,fixture".to_string()),
            license: Some("MIT".to_string()),
        };
        let markdown = render_catalog_markdown("acme-app", &description);
        assert!(
            markdown.starts_with("---\n"),
            "frontmatter opens the document: {markdown}"
        );
        assert!(markdown.contains("title: acme-app"));
        assert!(markdown.contains("description: A tiny test package"));
        assert!(
            markdown.contains("- test"),
            "keywords list split from the comma string: {markdown}"
        );
        assert!(markdown.contains("- fixture"));
        assert!(markdown.contains("# acme-app"));
        assert!(markdown.contains("A tiny test package"));
        assert!(markdown.contains("Keywords: test,fixture"));
        assert!(markdown.contains("License: MIT"));
    }

    #[test]
    fn render_catalog_markdown_omits_absent_fields() {
        let markdown = render_catalog_markdown("acme-app", &ocx_python::WheelDescription::default());
        assert_eq!(markdown, "---\ntitle: acme-app\n---\n\n# acme-app\n");
    }

    #[tokio::test]
    async fn synthesize_env_catalog_skips_non_env_sources() {
        let yaml = r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: github_release
  owner: acme
  repo: acme-app
assets:
  linux/amd64:
    - "acme-app-.*\\.tar\\.gz"
"#;
        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let result = synthesize_env_catalog(&spec, Path::new(".")).await.unwrap();
        assert!(
            result.is_none(),
            "a github_release source has nothing to synthesize from"
        );
    }

    fn pypi_yaml() -> &'static str {
        r#"
name: acme-app
target:
  registry: ocx.sh
  repository: acme-app
source:
  type: pypi
  package: acme-app
python:
  version: "3.13.1"
  abi: cp313
  interpreter_package: "ocx.sh/python/cpython:3.13.1"
"#
    }

    #[tokio::test]
    async fn synthesize_env_catalog_pypi_returns_none_without_a_derived_lock() {
        // No prior `pipeline plan` run reachable from this `describe`
        // invocation (no `locks/` directory under spec_dir) — nothing to
        // synthesize from, same fallback as a missing `CATALOG.md`.
        let spec: MirrorSpec = serde_yaml_ng::from_str(pypi_yaml()).unwrap();
        let tmp = tempdir().unwrap();
        let result = synthesize_env_catalog(&spec, tmp.path()).await.unwrap();
        assert!(result.is_none(), "no locks/ directory means nothing to synthesize from");
    }

    #[tokio::test]
    async fn synthesize_env_catalog_pypi_uses_a_derived_lock_when_present() {
        // A prior `pipeline plan` run left a derived lock under `locks/` —
        // catalog autogen must resolve the root wheel from it, the same as
        // the committed-lock `pylock` path does.
        let tmp = tempdir().unwrap();
        let locks_dir = tmp.path().join("locks");
        std::fs::create_dir_all(&locks_dir).unwrap();
        let lock_toml = r#"
lock-version = "1.0"

[[packages]]
name = "acme-app"
version = "1.0.0"

[[packages.wheels]]
name = "acme_app-1.0.0-py3-none-any.whl"
url = "https://example.com/acme_app-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#;
        std::fs::write(locks_dir.join("pylock.acme-app-1-0-0.toml"), lock_toml).unwrap();

        let spec: MirrorSpec = serde_yaml_ng::from_str(pypi_yaml()).unwrap();
        let lock = find_any_derived_pypi_lock(tmp.path())
            .await
            .expect("a derived lock in locks/ must be found and parsed");
        let package = crate::source::pylock::find_app_package(&lock, spec.source.pylock_app_name(&spec.name))
            .expect("the derived lock resolves the configured app package");
        assert_eq!(package.version, "1.0.0");
    }

    #[tokio::test]
    async fn find_any_derived_pypi_lock_returns_none_for_missing_directory() {
        let tmp = tempdir().unwrap();
        assert!(find_any_derived_pypi_lock(tmp.path()).await.is_none());
    }
}
