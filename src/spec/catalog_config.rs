// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Catalog publishing configuration for the mirror tool.
//!
//! `pipeline describe` reads the optional `catalog:` block to discover the
//! README and logo files to publish to the registry as catalog metadata
//! (the `__ocx.desc` referrer tag).
//!
//! When the block is omitted, defaults pick up `CATALOG.md` and probe for
//! `logo.svg` then `logo.png` relative to the spec file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Catalog publishing settings.
///
/// All fields are optional; sensible defaults apply when omitted.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogConfig {
    /// Path to the README file, relative to the spec file. Defaults to
    /// `CATALOG.md`.
    pub readme: Option<PathBuf>,
    /// Path to an optional logo file, relative to the spec file. When unset,
    /// the resolver probes for `logo.svg` then `logo.png`.
    pub logo: Option<PathBuf>,
}

impl CatalogConfig {
    /// Resolve the README path against `spec_dir`, applying the default
    /// (`CATALOG.md`) when no explicit path is configured.
    pub fn resolved_readme(&self, spec_dir: &Path) -> PathBuf {
        match &self.readme {
            Some(p) => spec_dir.join(p),
            None => spec_dir.join("CATALOG.md"),
        }
    }

    /// Reject a configured `readme:`/`logo:` that resolves to nothing.
    ///
    /// The *default* is deliberately forgiving: a repository with no
    /// `CATALOG.md` yet is a valid configuration, and `pipeline describe`
    /// logs and exits 0 so the generated workflow is a no-op until catalog
    /// content lands. That reasoning does not extend to a path somebody
    /// wrote down. `catalog.readme: claude-code/README.md` in
    /// `claude-code/mirror.yml` resolves to `claude-code/claude-code/…` —
    /// the repository-root reading of a spec-directory-relative field — and
    /// the block had never once taken effect. Exit 65 here, beside
    /// `metadata.default`'s identical check, rather than an INFO line on a
    /// job nobody reads.
    pub fn validate(&self, spec_dir: &Path, errors: &mut Vec<String>) {
        for (field, configured) in [("readme", &self.readme), ("logo", &self.logo)] {
            let Some(relative) = configured else { continue };
            if !spec_dir.join(relative).exists() {
                errors.push(format!(
                    "catalog.{field}: file not found: {} (resolved against the spec's own \
                     directory, not the repository root)",
                    relative.display()
                ));
            }
        }
    }

    /// Resolve the logo path against `spec_dir`. Returns `Some` only when
    /// either an explicit path is configured or a default candidate
    /// (`logo.svg`, then `logo.png`) exists on disk.
    ///
    /// The probe order favors SVG over PNG so vector logos win when both are
    /// present; callers wanting PNG-only must set `catalog.logo: logo.png`
    /// explicitly.
    pub fn resolved_logo(&self, spec_dir: &Path) -> Option<PathBuf> {
        if let Some(p) = &self.logo {
            return Some(spec_dir.join(p));
        }
        for candidate in ["logo.svg", "logo.png"] {
            let path = spec_dir.join(candidate);
            if path.exists() {
                return Some(path);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn catalog_config_default_readme_is_catalog_md() {
        let cfg = CatalogConfig::default();
        let dir = Path::new("/mirrors/shfmt");
        assert_eq!(cfg.resolved_readme(dir), dir.join("CATALOG.md"));
    }

    #[test]
    fn catalog_config_resolved_readme_joins_spec_dir() {
        let cfg = CatalogConfig {
            readme: Some(PathBuf::from("docs/catalog.md")),
            logo: None,
        };
        let dir = Path::new("/mirrors/shfmt");
        assert_eq!(cfg.resolved_readme(dir), dir.join("docs/catalog.md"));
    }

    #[test]
    fn catalog_config_explicit_logo_path_resolves() {
        let cfg = CatalogConfig {
            readme: None,
            logo: Some(PathBuf::from("brand/logo.png")),
        };
        let dir = Path::new("/mirrors/shfmt");
        assert_eq!(cfg.resolved_logo(dir), Some(dir.join("brand/logo.png")));
    }

    #[test]
    fn catalog_config_logo_probe_returns_svg_when_present() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("logo.svg"), b"<svg/>").unwrap();
        std::fs::write(tmp.path().join("logo.png"), b"\x89PNG").unwrap();
        let cfg = CatalogConfig::default();
        assert_eq!(cfg.resolved_logo(tmp.path()), Some(tmp.path().join("logo.svg")));
    }

    #[test]
    fn catalog_config_logo_probe_falls_back_to_png() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("logo.png"), b"\x89PNG").unwrap();
        let cfg = CatalogConfig::default();
        assert_eq!(cfg.resolved_logo(tmp.path()), Some(tmp.path().join("logo.png")));
    }

    #[test]
    fn catalog_config_logo_probe_returns_none_when_neither_exists() {
        let tmp = tempdir().unwrap();
        let cfg = CatalogConfig::default();
        assert_eq!(cfg.resolved_logo(tmp.path()), None);
    }

    #[test]
    fn a_configured_readme_that_resolves_to_nothing_is_an_error() {
        // The multi-spec trap: a repository-root-relative path under a
        // spec-directory-relative key. It resolved to
        // `<spec_dir>/<spec_dir>/README.md`, `describe` skipped it with an
        // INFO line and exited 0, and the block had never taken effect.
        let tmp = tempdir().unwrap();
        let cfg = CatalogConfig {
            readme: Some(PathBuf::from("claude-code/README.md")),
            logo: None,
        };
        let mut errors = Vec::new();
        cfg.validate(tmp.path(), &mut errors);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("catalog.readme") && errors[0].contains("claude-code/README.md"),
            "unexpected message: {errors:?}"
        );
    }

    #[test]
    fn a_configured_logo_that_resolves_to_nothing_is_an_error() {
        let tmp = tempdir().unwrap();
        let cfg = CatalogConfig {
            readme: None,
            logo: Some(PathBuf::from("../logo.svg")),
        };
        let mut errors = Vec::new();
        cfg.validate(tmp.path(), &mut errors);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("catalog.logo"), "unexpected message: {errors:?}");
    }

    #[test]
    fn an_unset_catalog_block_stays_silent() {
        // No `CATALOG.md`, no `logo.*`, and that is a valid spec: the
        // describe workflow is a no-op until catalog content lands.
        let tmp = tempdir().unwrap();
        let mut errors = Vec::new();
        CatalogConfig::default().validate(tmp.path(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn a_configured_path_that_exists_passes() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("catalog.md"), b"# x").unwrap();
        let cfg = CatalogConfig {
            readme: Some(PathBuf::from("catalog.md")),
            logo: None,
        };
        let mut errors = Vec::new();
        cfg.validate(tmp.path(), &mut errors);
        assert!(errors.is_empty(), "{errors:?}");
    }
}
