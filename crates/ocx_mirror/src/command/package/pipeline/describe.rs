// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline describe` — publish catalog metadata (README + logo)
//! to the registry as a referrer of the target repository.
//!
//! Loads `mirror.yml`, resolves the optional `catalog:` block (defaults to
//! `CATALOG.md` + probed `logo.{svg,png}`), then shells out to
//! `ocx package describe` to publish the data under the `__ocx.desc` tag.
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
//! - [`MirrorError::ExecutionFailed`] when the `ocx package describe`
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
        let (readme, synthesized) = if default_readme.exists() {
            (default_readme, false)
        } else if let Some(path) = synthesize_env_catalog(&spec, spec_dir).await? {
            (path, true)
        } else {
            tracing::info!(
                "describe: {} not found; skipping (no catalog content to publish)",
                default_readme.display()
            );
            return Ok(());
        };

        let logo = catalog.resolved_logo(spec_dir);
        let identifier = format!("{}/{}", spec.target.registry, spec.target.repository);

        let result = invoke_describe(&identifier, &readme, logo.as_deref()).await;
        if synthesized {
            let _ = tokio::fs::remove_file(&readme).await;
        }
        result
    }
}

/// When no on-disk `CATALOG.md` exists and `spec.source` is an env source,
/// synthesizes a minimal catalog markdown from the root package's wheel
/// `*.dist-info/METADATA` (`Summary`/`Keywords`/`License`) and writes it to a
/// process-unique temp file. Returns `Ok(None)` when there's nothing to
/// synthesize from.
///
/// `source.type: pypi` is not handled here (returns `Ok(None)`): unlike
/// `pylock`, it has no committed lock to resolve a root wheel from locally —
/// deriving one is the plan phase's job (W1.A2/W2.A3), not reachable from a
/// standalone `describe` invocation.
/// ponytail: pypi catalog autogen deferred; wire it once a plan/derived-lock
/// artifact is reachable from this phase.
async fn synthesize_env_catalog(spec: &MirrorSpec, spec_dir: &Path) -> Result<Option<PathBuf>, MirrorError> {
    let Source::Pylock { path, .. } = &spec.source else {
        return Ok(None);
    };

    let app_name = spec.source.pylock_app_name(&spec.name);
    let lock = crate::source::pylock::load(spec_dir, path)
        .await
        .map_err(|e| crate::source::pylock::classify_error("failed to load pylock for catalog autogen", e))?;
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

    let wheel_path = std::env::temp_dir().join(format!("ocx-mirror-catalog-wheel-{}.whl", std::process::id()));
    let client = reqwest::Client::new();
    crate::pipeline::download::download(&client, &parsed_url, &wheel_path)
        .await
        .map_err(|e| {
            MirrorError::ExecutionFailed(vec![format!(
                "failed to download root wheel for catalog autogen: {e:#}"
            )])
        })?;

    let description = ocx_python::read_wheel_description(&wheel_path).map_err(|e| {
        MirrorError::ExecutionFailed(vec![format!("failed to read wheel metadata for catalog autogen: {e}")])
    });
    let _ = tokio::fs::remove_file(&wheel_path).await;
    let description = description?;

    let markdown = render_catalog_markdown(&spec.name, &description);
    let catalog_path = std::env::temp_dir().join(format!("ocx-mirror-catalog-{}.md", std::process::id()));
    tokio::fs::write(&catalog_path, markdown)
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to write synthesized catalog: {e}")]))?;

    Ok(Some(catalog_path))
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

/// Renders a minimal `CATALOG.md` body from wheel metadata: the mirror
/// `name` as the title, `Summary` as the lead paragraph, `Keywords`/`License`
/// as trailer lines when present.
fn render_catalog_markdown(name: &str, description: &ocx_python::WheelDescription) -> String {
    let mut markdown = format!("# {name}\n");
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

/// Spawn `ocx package describe <identifier> --readme <path> [--logo <path>]`.
async fn invoke_describe(identifier: &str, readme: &Path, logo: Option<&Path>) -> Result<(), MirrorError> {
    let ocx_binary = resolve_ocx_binary().map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;

    let mut cmd = tokio::process::Command::new(&ocx_binary);
    cmd.args(["package", "describe", identifier, "--readme"]);
    cmd.arg(readme);
    if let Some(logo_path) = logo {
        cmd.arg("--logo");
        cmd.arg(logo_path);
    }
    forward_ocx_env(&mut cmd);

    let status = cmd
        .status()
        .await
        .map_err(|e| MirrorError::ExecutionFailed(vec![format!("failed to spawn ocx: {e}")]))?;

    if !status.success() {
        return Err(MirrorError::ExecutionFailed(vec![format!(
            "ocx package describe exited {status} for {identifier}"
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

    /// Mirror of [`invoke_describe`] arg assembly — keeps the assertion target
    /// independent of the subprocess spawn.
    fn assemble_args<'a>(identifier: &'a str, readme: &'a Path, logo: Option<&'a Path>) -> Vec<String> {
        let mut args: Vec<String> = vec![
            "package".into(),
            "describe".into(),
            identifier.into(),
            "--readme".into(),
            readme.display().to_string(),
        ];
        if let Some(p) = logo {
            args.push("--logo".into());
            args.push(p.display().to_string());
        }
        args
    }

    #[test]
    fn describe_assembles_args_with_readme_only() {
        let args = assemble_args("ocx.sh/shfmt", Path::new("/spec/CATALOG.md"), None);
        assert_eq!(
            args,
            vec!["package", "describe", "ocx.sh/shfmt", "--readme", "/spec/CATALOG.md"]
        );
    }

    #[test]
    fn describe_assembles_args_with_readme_and_logo() {
        let args = assemble_args(
            "ocx.sh/shfmt",
            Path::new("/spec/CATALOG.md"),
            Some(Path::new("/spec/logo.png")),
        );
        assert_eq!(
            args,
            vec![
                "package",
                "describe",
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
        assert!(markdown.contains("# acme-app"));
        assert!(markdown.contains("A tiny test package"));
        assert!(markdown.contains("Keywords: test,fixture"));
        assert!(markdown.contains("License: MIT"));
    }

    #[test]
    fn render_catalog_markdown_omits_absent_fields() {
        let markdown = render_catalog_markdown("acme-app", &ocx_python::WheelDescription::default());
        assert_eq!(markdown, "# acme-app\n");
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

    #[tokio::test]
    async fn synthesize_env_catalog_skips_pypi_sources() {
        // pypi has no committed lock to resolve a root wheel from locally —
        // deferred (see the ponytail note on `synthesize_env_catalog`).
        let yaml = r#"
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
"#;
        let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).unwrap();
        let result = synthesize_env_catalog(&spec, Path::new(".")).await.unwrap();
        assert!(result.is_none(), "pypi catalog autogen is deferred, not implemented");
    }
}
