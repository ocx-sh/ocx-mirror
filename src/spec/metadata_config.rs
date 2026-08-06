// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::bin_scan::BinScanMode;
use ocx_lib::package::metadata::authoring::AuthoringMetadata;
use ocx_lib::package::metadata::env::modifier::Modifier;
use ocx_lib::package::metadata::template::classify_install_path_rooted_dir;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct MetadataConfig {
    pub default: PathBuf,
    #[serde(default)]
    pub platforms: HashMap<String, PathBuf>,
}

impl MetadataConfig {
    pub fn validate(&self, spec_dir: &Path, errors: &mut Vec<String>) {
        let default_path = spec_dir.join(&self.default);
        if !default_path.exists() {
            errors.push(format!("metadata.default: file not found: {}", self.default.display()));
        }

        for (platform, path) in &self.platforms {
            let full_path = spec_dir.join(path);
            if !full_path.exists() {
                errors.push(format!(
                    "metadata.platforms.{platform}: file not found: {}",
                    path.display()
                ));
            }
        }
    }

    /// Rejects a `bin_scan` whose metadata gives the scan nowhere to look.
    ///
    /// The scan reads directories *below* `${installPath}`, so a metadata file
    /// whose interface-visible `Path` vars are all bare `${installPath}` — the
    /// usual `asset_type: binary` shape, one executable at the content root —
    /// yields zero scan targets, and `auto` then bakes `binaries: []`: a
    /// positive published claim that the package exposes no executables. That
    /// is wrong metadata reaching a real registry silently, so it fails the
    /// spec load instead.
    ///
    /// Every file the config can select is checked, not just `default`: a
    /// per-platform override with no scan target publishes the empty claim on
    /// exactly that platform.
    ///
    /// `label` names the variant so a multi-variant spec says which one.
    pub fn validate_scannable(&self, spec_dir: &Path, label: &str, mode: BinScanMode, errors: &mut Vec<String>) {
        for path in std::iter::once(&self.default).chain(self.platforms.values()) {
            // A missing or unparseable file is already reported by `validate`.
            let Ok(text) = std::fs::read_to_string(spec_dir.join(path)) else {
                continue;
            };
            let Ok(metadata) = serde_json::from_str::<AuthoringMetadata>(&text) else {
                continue;
            };
            // `auto` passes a declared list through *without scanning*, so the
            // scan target is irrelevant there and rejecting would block a
            // legitimate spec. `verify` is not the same: it walks the tree, and
            // with no scan target `verify_declared_binaries` iterates an empty
            // candidate map — both its loops no-op and it returns `Ok`, so the
            // verification can never fail. A `verify` that cannot go red is
            // worse than no verification, because the spec says it is checked.
            if metadata.binaries().is_some() && mode == BinScanMode::Auto {
                continue;
            }
            if !has_scan_target(&metadata) {
                let consequence = match metadata.binaries().is_some() {
                    true => "the verification would inspect no file and pass green whatever the archive contains",
                    false => {
                        "the scan would find nothing and publish `binaries: []`, a claim that the package \
                              exposes no executables"
                    }
                };
                errors.push(format!(
                    "{label}: bin_scan is enabled but {} declares no interface-visible \
                     ${{installPath}}/<dir> PATH entry — {consequence}. \
                     Point a PATH entry at a subdirectory, or set bin_scan: off and list binaries by hand.",
                    path.display()
                ));
            }
        }
    }
}

/// Whether a bin-scan of this metadata would have any directory to read.
///
/// ponytail: mirrors the scan-scope rule in ocx's `bin_scan::collect_candidates`
/// (interface visibility + an `${installPath}/<dir>`-shaped `Path` var) rather
/// than calling it, because that function needs an extracted tree and this runs
/// at spec load. Six lines against a stable ADR-pinned rule; if the rule grows a
/// third condition, ocx should export the predicate and this should call it.
fn has_scan_target(metadata: &AuthoringMetadata) -> bool {
    let AuthoringMetadata::Bundle(bundle) = metadata;
    (&bundle.env).into_iter().any(|var| {
        var.visibility.has_interface()
            && matches!(&var.modifier, Modifier::Path(path) if classify_install_path_rooted_dir(&path.value).is_some())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// YAML round-trip: a multi-platform `metadata:` block with per-platform
    /// override files must parse and produce a `MetadataConfig` where each
    /// platform key resolves to the correct override file.
    ///
    /// This test guards against the class of bug where YAML keys containing `/`
    /// (the OCI platform separator, e.g. `darwin/arm64`) are silently dropped or
    /// mangled during deserialization, causing all platforms to fall back to the
    /// default metadata and thus embedding `${installPath}/bin` instead of the
    /// platform-specific `${installPath}/CMake.app/Contents/bin` into darwin
    /// bundles.
    #[test]
    fn metadata_config_yaml_round_trip_with_slash_keys() {
        let yaml = r#"
default: metadata.json
platforms:
  windows/amd64: metadata-windows.json
  windows/arm64: metadata-windows.json
  darwin/amd64: metadata-darwin.json
  darwin/arm64: metadata-darwin.json
"#;
        let config: MetadataConfig = serde_yaml_ng::from_str(yaml)
            .expect("MetadataConfig must deserialize from YAML with slash-containing platform keys");

        assert_eq!(config.default, PathBuf::from("metadata.json"));

        // Each platform must be present and map to the correct file.
        for (platform, expected_file) in &[
            ("windows/amd64", "metadata-windows.json"),
            ("windows/arm64", "metadata-windows.json"),
            ("darwin/amd64", "metadata-darwin.json"),
            ("darwin/arm64", "metadata-darwin.json"),
        ] {
            let actual = config.platforms.get(*platform).unwrap_or_else(|| {
                panic!(
                    "platforms map must contain '{platform}' key; got keys: {:?}",
                    config.platforms.keys().collect::<Vec<_>>()
                )
            });
            assert_eq!(
                actual,
                &PathBuf::from(*expected_file),
                "platform '{platform}' must map to '{expected_file}'"
            );
        }

        assert_eq!(
            config.platforms.len(),
            4,
            "must have exactly 4 platform overrides; got: {:?}",
            config.platforms.keys().collect::<Vec<_>>()
        );
    }

    // ── bin_scan scannability gate ────────────────────────────────────────

    /// Writes `name` with one interface-visible PATH entry set to `value`.
    fn write_metadata(dir: &Path, name: &str, value: &str) {
        std::fs::write(
            dir.join(name),
            format!(
                r#"{{"type":"bundle","version":1,"env":[
                   {{"key":"PATH","type":"path","required":true,"value":"{value}","visibility":"public"}}]}}"#
            ),
        )
        .expect("write metadata fixture");
    }

    fn scan_errors(dir: &Path, config: &MetadataConfig) -> Vec<String> {
        scan_errors_for(dir, config, BinScanMode::Auto)
    }

    fn scan_errors_for(dir: &Path, config: &MetadataConfig, mode: BinScanMode) -> Vec<String> {
        let mut errors = Vec::new();
        config.validate_scannable(dir, "bin_scan", mode, &mut errors);
        errors
    }

    /// Writes `name` declaring `binaries` alongside one PATH entry.
    fn write_declared_metadata(dir: &Path, name: &str, value: &str) {
        std::fs::write(
            dir.join(name),
            format!(
                r#"{{"type":"bundle","version":1,"binaries":["shfmt"],"env":[
                   {{"key":"PATH","type":"path","required":true,"value":"{value}","visibility":"public"}}]}}"#
            ),
        )
        .expect("write metadata fixture");
    }

    fn config(default: &str, platforms: &[(&str, &str)]) -> MetadataConfig {
        MetadataConfig {
            default: default.into(),
            platforms: platforms
                .iter()
                .map(|(key, file)| ((*key).to_string(), PathBuf::from(*file)))
                .collect(),
        }
    }

    /// The footgun this gate exists for: the scan reads directories *below*
    /// `${installPath}`, so a bare `${installPath}` PATH entry — an
    /// `asset_type: binary` mirror's usual shape — gives it nothing to read and
    /// `auto` bakes `binaries: []`, a positive claim that the package exposes
    /// no executables. Measured against ocx before this gate existed; it must
    /// fail the load, not reach a registry.
    #[test]
    fn a_bare_install_path_var_is_rejected_when_the_scan_is_on() {
        let dir = tempfile::TempDir::new().unwrap();
        write_metadata(dir.path(), "metadata.json", "${installPath}");

        let errors = scan_errors(dir.path(), &config("metadata.json", &[]));
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(
            errors[0].contains("binaries: []") && errors[0].contains("metadata.json"),
            "the error must name the file and the claim it would publish: {}",
            errors[0],
        );
    }

    /// The sibling that must keep loading — `${installPath}/bin` is the shape
    /// every archive mirror uses, and `mirror-cmake` ships exactly this.
    #[test]
    fn an_install_path_rooted_dir_var_passes() {
        let dir = tempfile::TempDir::new().unwrap();
        write_metadata(dir.path(), "metadata.json", "${installPath}/bin");

        assert!(scan_errors(dir.path(), &config("metadata.json", &[])).is_empty());
    }

    /// Every file the config can select is checked, not just `default`: a
    /// per-platform override with no scan target publishes the empty claim on
    /// exactly that platform and nowhere else — the hardest version to notice.
    #[test]
    fn a_per_platform_override_without_a_scan_target_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        write_metadata(dir.path(), "metadata.json", "${installPath}/bin");
        write_metadata(dir.path(), "metadata-windows.json", "${installPath}");

        let errors = scan_errors(
            dir.path(),
            &config("metadata.json", &[("windows/amd64", "metadata-windows.json")]),
        );
        assert_eq!(errors.len(), 1, "got: {errors:?}");
        assert!(errors[0].contains("metadata-windows.json"), "got: {}", errors[0]);
    }

    /// The declared-list carve-out is `auto`-only, and the two halves must be
    /// asserted together or the gate silently loses one mode.
    ///
    /// `auto` passes a declared list through *without scanning*, so the target
    /// never matters and rejecting blocks a legitimate spec. `verify` does walk
    /// the tree — with no scan target `verify_declared_binaries` iterates an
    /// empty candidate map, both loops no-op, and it returns `Ok`. So a
    /// carve-out that covered `verify` too produced a spec whose declared
    /// verification could never go red, which is worse than not verifying:
    /// the spec claims the list is checked.
    #[test]
    fn the_declared_list_carve_out_is_auto_only() {
        let dir = tempfile::TempDir::new().unwrap();
        write_declared_metadata(dir.path(), "metadata.json", "${installPath}");
        let config = config("metadata.json", &[]);

        assert!(
            scan_errors_for(dir.path(), &config, BinScanMode::Auto).is_empty(),
            "auto never scans a declared claim, so it needs no scan target",
        );

        let errors = scan_errors_for(dir.path(), &config, BinScanMode::Verify);
        assert_eq!(
            errors.len(),
            1,
            "verify with nothing to inspect must be rejected: {errors:?}"
        );
        assert!(
            errors[0].contains("inspect no file"),
            "the error must say the verification is vacuous, not that a scan would publish []: {}",
            errors[0],
        );
    }

    /// And a declared list *with* a scan target loads under both modes — the
    /// control that keeps the test above from passing on a gate that simply
    /// rejects every `verify`.
    #[test]
    fn a_declared_list_with_a_scan_target_loads_under_both_modes() {
        let dir = tempfile::TempDir::new().unwrap();
        write_declared_metadata(dir.path(), "metadata.json", "${installPath}/bin");
        let config = config("metadata.json", &[]);

        for mode in [BinScanMode::Auto, BinScanMode::Verify] {
            assert!(
                scan_errors_for(dir.path(), &config, mode).is_empty(),
                "{mode:?} with a real scan target must load",
            );
        }
    }

    /// A private-visibility PATH var is never a scan target (ADR §1), so it
    /// must not satisfy the gate either — otherwise a `libexec`-only spec
    /// passes validation and still publishes `binaries: []`.
    #[test]
    fn a_private_visibility_var_does_not_count_as_a_scan_target() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("metadata.json"),
            r#"{"type":"bundle","version":1,"env":[
               {"key":"PATH","type":"path","required":true,"value":"${installPath}/libexec","visibility":"private"}]}"#,
        )
        .unwrap();

        assert_eq!(scan_errors(dir.path(), &config("metadata.json", &[])).len(), 1);
    }

    /// `resolve_metadata` selects `metadata-darwin.json` for `darwin/arm64`
    /// when the platforms map contains that key.
    ///
    /// Pre-fix: if YAML deserialization dropped slash-containing keys, the
    /// map would be empty and this call would always return the default
    /// metadata — the exact bug that caused darwin bundles to embed
    /// `${installPath}/bin` instead of `${installPath}/CMake.app/Contents/bin`.
    #[test]
    fn resolve_metadata_selects_darwin_override_from_yaml_config() {
        use crate::pipeline::package;

        let dir = tempfile::TempDir::new().unwrap();
        let default_content = r#"{"type":"bundle","version":1,"env":[{"key":"PATH","type":"path","required":true,"value":"${installPath}/bin","visibility":"public"}]}"#;
        let darwin_content = r#"{"type":"bundle","version":1,"env":[{"key":"PATH","type":"path","required":true,"value":"${installPath}/CMake.app/Contents/bin","visibility":"public"}]}"#;
        std::fs::write(dir.path().join("metadata.json"), default_content).unwrap();
        std::fs::write(dir.path().join("metadata-darwin.json"), darwin_content).unwrap();

        let yaml = r#"
default: metadata.json
platforms:
  darwin/amd64: metadata-darwin.json
  darwin/arm64: metadata-darwin.json
"#;
        let config: MetadataConfig = serde_yaml_ng::from_str(yaml).unwrap();

        for platform in &["darwin/amd64", "darwin/arm64"] {
            let metadata = package::resolve_metadata(&config, platform, dir.path())
                .unwrap_or_else(|e| panic!("resolve_metadata failed for {platform}: {e}"));

            // The resolved metadata must declare the darwin-specific PATH value,
            // not the default `${installPath}/bin`. `env` lives on the published
            // projection; the authoring form this returns is the publisher's
            // hand-written shape.
            let published = metadata
                .to_published()
                .unwrap_or_else(|e| panic!("to_published failed for {platform}: {e}"));
            let path_entry = published
                .env()
                .and_then(|vars| vars.into_iter().find(|v| v.key == "PATH"))
                .unwrap_or_else(|| panic!("PATH entry missing in metadata for {platform}"));

            let path_value = path_entry
                .value()
                .unwrap_or_else(|| panic!("PATH entry has no value for {platform}"));

            assert_eq!(
                path_value, "${installPath}/CMake.app/Contents/bin",
                "darwin platform '{platform}' must use metadata-darwin.json (CMake.app path), \
                 not the default bin/ path; got: {path_value:?}\n\
                 pre-fix regression: YAML slash-key deserialization must populate platforms map"
            );
        }
    }
}
