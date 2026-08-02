// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::path::Path;

use anyhow::Result;
use ocx_lib::archive::{Archive, ExtractOptions};
use ocx_lib::oci::Platform;
use ocx_lib::package::bundle::BundleBuilder;
use ocx_lib::package::metadata::authoring::AuthoringMetadata;
use ocx_lib::package::metadata::binary::Binaries;

use crate::spec::{AssetType, MetadataConfig};

/// Lay a downloaded asset out into `content_dir` — the tree a bundle is made
/// of, and the tree a `bin_scan` reads.
///
/// The [`AssetType`] determines how the asset is handled:
/// - `Archive`: extracted as a tar/zip, with optional `strip_components`.
/// - `Binary`: placed directly into the content directory under the configured name.
///
/// Separate from [`bundle`] because the published metadata is finalised
/// between the two: `bin_scan` derives the `binaries` claim from this tree, and
/// the sidecar carrying it must be written before the bundle exists, or a run
/// interrupted in between would resume off a bundle with no record of what was
/// scanned.
pub async fn extract(asset_path: &Path, content_dir: &Path, asset_type: &AssetType, asset_name: &str) -> Result<()> {
    match asset_type {
        AssetType::Archive { strip_components } => {
            let options = strip_components.map(|sc| ExtractOptions {
                strip_components: sc as usize,
                ..Default::default()
            });
            Archive::extract_with_options(asset_path, content_dir, options).await?;
        }
        AssetType::Binary { name } => {
            place_binary(asset_path, content_dir, name, asset_name).await?;
        }
    }
    Ok(())
}

/// Compress an extracted `content_dir` into the OCX bundle at `bundle_path`.
///
/// `compression_threads` is passed directly to `CompressionOptions::with_threads()`.
/// `0` = auto-detect, `1` = single-threaded, `n` = use n threads.
pub async fn bundle(content_dir: &Path, bundle_path: &Path, compression_threads: u32) -> Result<()> {
    BundleBuilder::from_path(content_dir)
        .with_compression(ocx_lib::compression::CompressionOptions::default().with_threads(compression_threads))
        .create(bundle_path)
        .await?;
    Ok(())
}

/// Place a binary into the content directory and make it executable.
///
/// The filename is the configured `name` from the spec. If the downloaded asset
/// has a `.exe` extension, it is preserved on the output filename.
async fn place_binary(asset_path: &Path, content_dir: &Path, name: &str, asset_name: &str) -> Result<()> {
    let filename = if asset_name.ends_with(".exe") && !name.ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    let dest = content_dir.join(&filename);
    tokio::fs::copy(asset_path, &dest).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o755);
        tokio::fs::set_permissions(&dest, perms).await?;
    }

    Ok(())
}

/// Make every file in `content_dir` whose name is a declared binary executable.
///
/// A tar or zip member carries the mode upstream gave it, so an archive mirror
/// publishes whatever the release tarball happened to hold — and some upstreams
/// ship their interface binary at 0644. The `binaries` list is the one statement
/// of which files are commands, so it is what this is keyed on.
///
/// The whole tree is walked rather than the metadata's interface `PATH`
/// directories, because the layout this exists for has none: the bug's own spec
/// puts the binary at the archive root under a bare `${installPath}` PATH entry.
///
/// A declared name absent from the tree is not an error here. `bin_scan: verify`
/// is the presence guard where one is usable at all; failing on absence would
/// red every mirror whose list covers several platforms' file sets.
///
/// The `& 0o111` skip makes this strictly "make executable if it is not" — a
/// file that already carries any exec bit keeps its exact mode, so this never
/// downgrades a 0775 or a setuid bit to 0755.
///
/// ponytail: two `read_dir`s per directory (the walk's own, then the scan's),
/// negligible beside the xz compression that follows. Fold the file listing into
/// the walk only if a huge extracted tree measurably shows up.
pub async fn ensure_declared_binaries_executable(content_dir: &Path, binaries: &Binaries) -> Result<()> {
    #[cfg(unix)]
    {
        use std::collections::BTreeSet;
        use std::os::unix::fs::PermissionsExt;

        use anyhow::Context;
        use ocx_lib::package::bin_scan;
        use ocx_lib::utility::fs::{DirWalker, WalkDecision};

        let declared: BTreeSet<&str> = binaries.iter().map(|name| name.as_str()).collect();
        let directories = DirWalker::new(content_dir, |directory: &Path, _depth| {
            WalkDecision::collect_and_descend(vec![directory.to_path_buf()])
        })
        .walk()
        .await?;

        for directory in directories {
            for (path, file_metadata) in bin_scan::scan_directory_files(&directory).await? {
                let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                if !declared.contains(file_name) || file_metadata.permissions().mode() & 0o111 != 0 {
                    continue;
                }
                tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .await
                    .with_context(|| format!("failed to make declared binary {} executable", path.display()))?;
            }
        }
    }
    #[cfg(not(unix))]
    {
        // The exec bit is a POSIX concept with no host API here; the walk would
        // be pure cost.
        let _ = (content_dir, binaries);
    }
    Ok(())
}

/// Resolve the metadata JSON file for a given platform, falling back to the default.
///
/// Returns the *authoring* form: a spec's `metadata.json` is what a publisher
/// hand-writes, and it may carry sidecar-only fields (per-dependency
/// `platforms` pin maps) that the published form has no room for.
pub fn resolve_metadata(config: &MetadataConfig, platform: &str, spec_dir: &Path) -> Result<AuthoringMetadata> {
    let metadata_path = if let Some(platform_path) = config.platforms.get(platform) {
        spec_dir.join(platform_path)
    } else {
        spec_dir.join(&config.default)
    };

    let content = std::fs::read_to_string(&metadata_path)
        .map_err(|e| anyhow::anyhow!("failed to read metadata file {}: {e}", metadata_path.display()))?;

    let metadata: AuthoringMetadata = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("failed to parse metadata file {}: {e}", metadata_path.display()))?;

    Ok(metadata)
}

/// Renders the `-metadata.json` sidecar that travels beside a prepared bundle.
///
/// The recorded `platform` is the whole point: `ocx package test` and `ocx
/// package push` both resolve a bundle's platform from its sidecar and reject
/// one that has none, so a sidecar written without it fails downstream with
/// "metadata sidecar has no recorded platform". `ocx package create` records
/// it via the same [`AuthoringMetadata::with_platform`]; the spec-level
/// `metadata.json` a publisher writes by hand never carries it, because the
/// spec is platform-independent and one file serves every platform.
pub fn sidecar_json(metadata: &AuthoringMetadata, platform: &Platform) -> Result<String> {
    let recorded = metadata.clone().with_platform(platform.clone());
    serde_json::to_string_pretty(&recorded)
        .map_err(|e| anyhow::anyhow!("failed to serialize metadata sidecar for {platform}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn place_binary_uses_configured_name() {
        let dir = tempfile::TempDir::new().unwrap();
        let asset = dir.path().join("shfmt_v3.13.0_linux_amd64");
        std::fs::write(&asset, b"fake binary").unwrap();

        let content_dir = dir.path().join("content");
        std::fs::create_dir(&content_dir).unwrap();

        place_binary(&asset, &content_dir, "shfmt", "shfmt_v3.13.0_linux_amd64")
            .await
            .unwrap();

        let dest = content_dir.join("shfmt");
        assert!(dest.exists());
        assert_eq!(std::fs::read(&dest).unwrap(), b"fake binary");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
            assert_eq!(mode & 0o755, 0o755);
        }
    }

    #[tokio::test]
    async fn place_binary_appends_exe_for_windows_assets() {
        let dir = tempfile::TempDir::new().unwrap();
        let asset = dir.path().join("shfmt_v3.13.0_windows_amd64.exe");
        std::fs::write(&asset, b"fake exe").unwrap();

        let content_dir = dir.path().join("content");
        std::fs::create_dir(&content_dir).unwrap();

        place_binary(&asset, &content_dir, "shfmt", "shfmt_v3.13.0_windows_amd64.exe")
            .await
            .unwrap();

        assert!(content_dir.join("shfmt.exe").exists());
    }

    /// The four properties the chmod is keyed on: it reaches a nested directory
    /// (full-tree walk, not the interface `PATH` dirs), it touches only the
    /// names the metadata declares, it leaves an already-executable file's exact
    /// mode alone rather than flattening it to 0755, and a declared name the
    /// archive does not ship is silently skipped rather than failing the run.
    #[cfg(unix)]
    #[tokio::test]
    async fn declared_binaries_are_made_executable_anywhere_in_the_tree() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        let content_dir = dir.path().join("content");
        std::fs::create_dir_all(content_dir.join("nested")).unwrap();
        for (relative, mode) in [("nested/tool", 0o644), ("data", 0o644), ("already", 0o775)] {
            let file = content_dir.join(relative);
            std::fs::write(&file, b"payload").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        let binaries: Binaries = serde_json::from_str(r#"["tool","already","absent"]"#).unwrap();
        ensure_declared_binaries_executable(&content_dir, &binaries)
            .await
            .expect("a declared name absent from the tree must not fail the run");

        let mode = |relative: &str| {
            std::fs::metadata(content_dir.join(relative))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("nested/tool"), 0o755, "the walk must reach nested directories");
        assert_eq!(mode("data"), 0o644, "an undeclared file must keep its mode");
        assert_eq!(
            mode("already"),
            0o775,
            "a file that is already executable must keep its exact mode, not be flattened to 0755",
        );
    }

    #[tokio::test]
    async fn extract_and_bundle_excludes_metadata_from_content() {
        use ocx_lib::archive::Archive;

        let dir = tempfile::TempDir::new().unwrap();

        let asset = dir.path().join("shfmt_v3.13.0_linux_amd64");
        std::fs::write(&asset, b"fake binary").unwrap();

        let content_dir = dir.path().join("content");
        std::fs::create_dir(&content_dir).unwrap();

        let bundle_path = dir.path().join("bundle.tar.xz");
        let asset_type = AssetType::Binary { name: "shfmt".into() };

        extract(&asset, &content_dir, &asset_type, "shfmt_v3.13.0_linux_amd64")
            .await
            .unwrap();
        bundle(&content_dir, &bundle_path, 1).await.unwrap();

        // Extract the published bundle and inspect its contents. The bundle is a
        // tar of `content_dir`'s entries at the archive root (the `content/`
        // prefix is added by install, not the mirror).
        let extracted = dir.path().join("extracted");
        Archive::extract(&bundle_path, &extracted).await.unwrap();

        assert!(extracted.join("shfmt").exists(), "tool payload missing from bundle");
        assert!(
            !extracted.join("metadata.json").exists(),
            "metadata.json must not be baked into bundle content",
        );
    }

    #[test]
    fn resolve_metadata_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let metadata_content = r#"{"type":"bundle","version":1,"strip_components":1,"env":[]}"#;
        std::fs::write(dir.path().join("default.json"), metadata_content).unwrap();

        let config = MetadataConfig {
            default: "default.json".into(),
            platforms: HashMap::new(),
        };

        let _metadata = resolve_metadata(&config, "linux/amd64", dir.path()).unwrap();
    }

    /// The sidecar `pipeline prepare` writes must record its platform.
    ///
    /// A spec-level `metadata.json` never carries one, and byte-copying it
    /// produced a sidecar that `ocx package test` and `ocx package push` both
    /// reject with "metadata sidecar has no recorded platform" — the whole
    /// mirror fleet's test leg, on the first version it actually had to test.
    /// Asserted through `resolve_platform`, the call that did the rejecting.
    #[test]
    fn sidecar_records_the_platform_it_was_prepared_for() {
        let dir = tempfile::TempDir::new().unwrap();
        // Exactly the shape a publisher hand-writes: no `platform` key.
        std::fs::write(
            dir.path().join("default.json"),
            r#"{"type":"bundle","version":1,"strip_components":1,"env":[]}"#,
        )
        .unwrap();
        let config = MetadataConfig {
            default: "default.json".into(),
            platforms: HashMap::new(),
        };
        let platform: Platform = "linux/arm64".parse().unwrap();

        let metadata = resolve_metadata(&config, "linux/arm64", dir.path()).unwrap();
        assert!(
            metadata.platform().is_none(),
            "fixture must start without a platform, or this test proves nothing"
        );

        let sidecar = sidecar_json(&metadata, &platform).unwrap();
        let written: AuthoringMetadata = serde_json::from_str(&sidecar).unwrap();

        assert_eq!(
            written.resolve_platform(Some(&platform)).unwrap(),
            platform,
            "sidecar must resolve to the platform it was prepared for"
        );
    }

    #[test]
    fn resolve_metadata_platform_override() {
        let dir = tempfile::TempDir::new().unwrap();
        let default_content = r#"{"type":"bundle","version":1,"strip_components":1,"env":[]}"#;
        let darwin_content = r#"{"type":"bundle","version":1,"strip_components":2,"env":[]}"#;
        std::fs::write(dir.path().join("default.json"), default_content).unwrap();
        std::fs::write(dir.path().join("darwin.json"), darwin_content).unwrap();

        let config = MetadataConfig {
            default: "default.json".into(),
            platforms: HashMap::from([("darwin/arm64".to_string(), "darwin.json".into())]),
        };

        let _metadata = resolve_metadata(&config, "darwin/arm64", dir.path()).unwrap();
    }
}
