// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::path::Path;

use anyhow::{Context, Result};
use ocx_oci::Platform;
use ocx_package::bundle::BundleBuilder;
use ocx_package::metadata::authoring::AuthoringMetadata;
use ocx_package::metadata::binary::Binaries;
use ocx_util::archive::{Archive, ExtractOptions};
use ocx_util::compression::CompressionAlgorithm;

use ocx_mirror_spec::{AssetType, MetadataConfig};

/// Lay a downloaded asset out into `content_dir` — the tree a bundle is made
/// of, and the tree a `bin_scan` reads.
///
/// The [`AssetType`] determines how the asset is handled:
/// - `Archive`: extracted as a tar/zip, with optional `strip_components`.
/// - `Binary`: placed directly into the content directory under the configured name,
///   decompressed first when the asset is a bare `.gz`/`.xz`/`.zst`/`.bz2` file.
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
        .with_compression(ocx_util::compression::CompressionOptions::default().with_threads(compression_threads))
        .create(bundle_path)
        .await?;
    Ok(())
}

/// Place a binary into the content directory and make it executable.
///
/// The filename is the configured `name` from the spec. If the downloaded asset
/// has a `.exe` extension, it is preserved on the output filename.
///
/// A bare single-file compressed asset (`taplo-linux-x86_64.gz`, no tar layer)
/// is decompressed first — detected by magic, not extension, since no real
/// executable starts with a compressor header. Copying it verbatim published a
/// gzip stream as the executable, which `execve` refuses (ocx-sh/ocx#284).
///
/// ponytail: no output size cap, parity with the archive path's extraction.
/// Add one to both if a decompression bomb from upstream ever matters.
async fn place_binary(asset_path: &Path, content_dir: &Path, name: &str, asset_name: &str) -> Result<()> {
    let compression = CompressionAlgorithm::from_file_magic(asset_path).await?;
    // `tool.exe.gz` keeps its `.exe`: test the name the asset has once decoded.
    let decoded_name = match compression {
        Some(_) => Path::new(asset_name)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(asset_name),
        None => asset_name,
    };
    let filename = if decoded_name.ends_with(".exe") && !name.ends_with(".exe") {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    let dest = content_dir.join(&filename);
    match compression {
        Some(algorithm) => {
            let mut reader = ocx_util::compression::read_file(asset_path, Some(algorithm)).await?;
            let decode_dest = dest.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
                std::io::copy(&mut reader, &mut std::fs::File::create(&decode_dest)?)
            })
            .await?
            .with_context(|| format!("failed to decompress {algorithm} asset {asset_name}"))?;
        }
        None => {
            tokio::fs::copy(asset_path, &dest).await?;
        }
    }

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
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        use ocx_package::bin_scan;
        use ocx_util::fs::{DirWalker, WalkDecision};

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
                // Two shapes of declared name reach a file outside the content
                // root, and both are reachable from an upstream archive: the
                // scan stats through symlinks and `chmod(2)` follows them, while
                // the extractor's symlink validation is purely lexical, so a
                // chain of parent links passes it; and a tar member of type
                // `Link` is unpacked with the raw linkname, so an absolute one
                // lands as an ordinary regular file sharing the victim's inode
                // (ocx-sh/ocx#275). The hardlink is the worse of the two — a
                // symlinked escape is caught by `validate_symlinks_in_dir`
                // before bundling, but that sweep sees a hardlink as the plain
                // regular file it is, so the escaped content gets published.
                //
                // Skipping does cost a real case: a declared name that is a
                // symlink is skipped outright, so an alias layout whose target
                // has a different basename (`bin/pwsh` → `libexec/pwsh.bin`)
                // leaves that target at whatever mode upstream shipped — the
                // walk reaches the target but does not recognise its name. The
                // conservative skip is still the right default; a containment
                // check is the fix if such a layout ever needs mirroring.
                //
                // `nlink > 1` cannot false-positive at any current call site,
                // because a relative linkname resolves against the process CWD
                // rather than the destination: an ordinary GNU-tar-dedup archive
                // fails extraction outright ("No such file or directory ... when
                // hard linking"), so every multiply-linked file under `content/`
                // arrived by escaping. That is a property of where we happen to
                // run, not an invariant anyone enforces — extracting with the
                // CWD set to the destination produces a legitimate in-tree pair,
                // which this clause would then skip. Two futures reopen it: ocx
                // resolving linknames under the destination (ocx-sh/ocx#275), or
                // any caller that chdirs. Real archives do ship hardlinked
                // binary shims (Kibana's `node_modules/.bin`), so the day
                // extraction stops rejecting them, this stops being a security
                // guard and starts silently skipping them — see
                // `an_in_tree_hardlink_pair_is_skipped_too`.
                let entry = match tokio::fs::symlink_metadata(&path).await {
                    Ok(entry) => entry,
                    // Same race `scan_directory_files` already tolerates: a file
                    // may vanish between the directory read and the stat.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        return Err(
                            anyhow::Error::new(e).context(format!("failed to stat declared binary {}", path.display()))
                        );
                    }
                };
                if entry.file_type().is_symlink() || entry.nlink() > 1 {
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
/// hand-writes, so it may leave a dependency's digest unresolved where the
/// published form requires one.
///
/// Two spellings a spec may still carry are now rejected here rather than
/// silently dropped, both from `ocx` 0.5.5: a top-level `platform` key, and a
/// per-dependency `platforms` pin map (a dependency carries its digest on the
/// identifier instead — `registry/repo:tag@sha256:…`). Either fails this parse,
/// so the spec dies at `pipeline plan`/`prepare` with the upstream migration
/// message naming the file, not later at push.
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
/// The sidecar carries no platform: `ocx` retired the top-level `platform` key
/// and now rejects a sidecar still bearing it. The platform reaches `ocx
/// package push` / `ocx package test` through their explicit `--platform` flag,
/// which every mirror invocation passes. `platform` here is error context only.
pub fn sidecar_json(metadata: &AuthoringMetadata, platform: &Platform) -> Result<String> {
    serde_json::to_string_pretty(metadata)
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

    /// Writes `payload` compressed with `algorithm` to `path`, the shape of a
    /// bare single-file upstream asset (`taplo-linux-x86_64.gz`).
    async fn write_compressed(path: &Path, algorithm: ocx_util::compression::CompressionAlgorithm, payload: &[u8]) {
        use std::io::Write;
        let options = ocx_util::compression::CompressionOptions::new(algorithm);
        let mut writer = ocx_util::compression::write_file(path, &options).await.unwrap();
        writer.write_all(payload).unwrap();
        // Dropping the writer finishes the stream's epilogue.
    }

    /// ocx-sh/ocx#284: a gzip'd raw ELF used to be copied verbatim — an
    /// "executable" that `execve` refuses with `Exec format error`.
    #[tokio::test]
    async fn place_binary_decompresses_a_bare_compressed_asset() {
        use ocx_util::compression::CompressionAlgorithm;

        let payload = b"\x7fELF fake binary payload";
        for (algorithm, extension) in [
            (CompressionAlgorithm::Gzip, "gz"),
            (CompressionAlgorithm::Lzma, "xz"),
            (CompressionAlgorithm::Zstd, "zst"),
        ] {
            let dir = tempfile::TempDir::new().unwrap();
            let asset_name = format!("taplo-linux-x86_64.{extension}");
            let asset = dir.path().join(&asset_name);
            write_compressed(&asset, algorithm, payload).await;

            let content_dir = dir.path().join("content");
            std::fs::create_dir(&content_dir).unwrap();

            place_binary(&asset, &content_dir, "taplo", &asset_name).await.unwrap();

            let dest = content_dir.join("taplo");
            assert_eq!(
                std::fs::read(&dest).unwrap(),
                payload,
                "{algorithm} asset shipped undecoded"
            );

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
                assert_eq!(mode & 0o755, 0o755);
            }
        }
    }

    #[tokio::test]
    async fn place_binary_keeps_exe_through_a_compression_suffix() {
        let dir = tempfile::TempDir::new().unwrap();
        let asset_name = "tool_windows_amd64.exe.gz";
        let asset = dir.path().join(asset_name);
        write_compressed(
            &asset,
            ocx_util::compression::CompressionAlgorithm::Gzip,
            b"MZ fake exe",
        )
        .await;

        let content_dir = dir.path().join("content");
        std::fs::create_dir(&content_dir).unwrap();

        place_binary(&asset, &content_dir, "tool", asset_name).await.unwrap();

        assert_eq!(std::fs::read(content_dir.join("tool.exe")).unwrap(), b"MZ fake exe");
    }

    #[tokio::test]
    async fn place_binary_refuses_a_corrupt_compressed_asset() {
        let dir = tempfile::TempDir::new().unwrap();
        let asset = dir.path().join("tool.gz");
        std::fs::write(&asset, b"\x1f\x8bnot a gzip stream").unwrap();

        let content_dir = dir.path().join("content");
        std::fs::create_dir(&content_dir).unwrap();

        place_binary(&asset, &content_dir, "tool", "tool.gz")
            .await
            .expect_err("a gzip header over garbage must fail prepare, not publish the bytes");
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

    /// A declared name that is a symlink must leave its target's mode alone.
    ///
    /// The extractor validates a member's link target *lexically* — no
    /// filesystem call — so an archive shipping self-referential parent links
    /// (`a`→`.`, `a/b`→`.`) has every declared path accepted while the file
    /// physically lands above the content root. Both the scan's stat and
    /// `chmod(2)` follow links, so the escapee is what got chmodded: an
    /// upstream archive picking the mode of an arbitrary file on the runner.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_declared_binary_that_is_a_symlink_leaves_its_target_alone() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        // Outside the walked tree — where the escape lands.
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"not upstream's to touch").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();

        let content_dir = dir.path().join("content");
        std::fs::create_dir(&content_dir).unwrap();
        std::os::unix::fs::symlink("../victim", content_dir.join("tool")).unwrap();

        let binaries: Binaries = serde_json::from_str(r#"["tool"]"#).unwrap();
        ensure_declared_binaries_executable(&content_dir, &binaries)
            .await
            .expect("a symlinked declared name must be skipped, not fail the run");

        assert_eq!(
            std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o600,
            "the chmod must not follow a declared binary's symlink out of the content root",
        );
    }

    /// A declared name that is a hardlink to a file outside the tree must leave
    /// that file's mode alone — the escape the symlink skip does not catch.
    ///
    /// Reproduced end-to-end against the real extractor before this test was
    /// written: a tar member of type `Link` whose linkname is absolute extracts
    /// with `Ok(())` and lands as an ordinary regular file at `nlink=2` sharing
    /// the victim's inode, so `is_symlink()` is false and the chmod took a 0600
    /// file outside the extraction root to 0755. The archive is built here with
    /// `hard_link` rather than a crafted tar: it produces the identical inode
    /// state, and the tar-side defect is upstream's (ocx-sh/ocx#275) — what this
    /// repository owns is that the chmod refuses a multiply-linked file.
    ///
    /// Unlike the symlink case, nothing downstream catches this one:
    /// `validate_symlinks_in_dir` sees a hardlink as the plain regular file it
    /// is, so without the skip the escaped content is bundled and published.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_declared_binary_that_is_a_hardlink_out_of_the_tree_leaves_its_target_alone() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::TempDir::new().unwrap();
        // Outside the walked tree — where the escape reaches.
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"not upstream's to touch").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();

        let content_dir = dir.path().join("content");
        std::fs::create_dir(&content_dir).unwrap();
        let escapee = content_dir.join("tool");
        std::fs::hard_link(&victim, &escapee).unwrap();

        let landed = std::fs::symlink_metadata(&escapee).unwrap();
        assert!(
            !landed.file_type().is_symlink() && landed.nlink() > 1,
            "the fixture must be a plain regular file the symlink skip cannot see, or this test proves nothing",
        );

        let binaries: Binaries = serde_json::from_str(r#"["tool"]"#).unwrap();
        ensure_declared_binaries_executable(&content_dir, &binaries)
            .await
            .expect("a hardlinked declared name must be skipped, not fail the run");

        assert_eq!(
            std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o600,
            "the chmod must not reach a declared binary's hardlink target outside the content root",
        );
    }

    /// The cost of `nlink > 1` being a heuristic rather than a containment
    /// check: a hardlink pair wholly *inside* the tree is skipped too, even
    /// though nothing escaped.
    ///
    /// Unreachable today — the extractor rejects any archive carrying a
    /// relative hardlink linkname before this runs — so this pins a limitation,
    /// not a behaviour anyone can currently observe. It is here as a tripwire:
    /// real upstreams ship hardlinked binary shims (Kibana's
    /// `node_modules/.bin`), so whoever makes those archives extractable —
    /// by fixing ocx-sh/ocx#275, or by extracting with a different CWD — will
    /// find this test asserting the declared binary is left non-executable,
    /// which is the bug #51 exists to prevent. Change it deliberately or
    /// replace the heuristic with containment; do not let it change silently.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_in_tree_hardlink_pair_is_skipped_too() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::TempDir::new().unwrap();
        let content_dir = dir.path().join("content");
        std::fs::create_dir_all(content_dir.join("bin")).unwrap();

        // Both ends inside the tree: the shape GNU tar emits when it dedups two
        // identical files, and the shape npm falls back to for a `.bin` shim.
        let original = content_dir.join("bin").join("tool");
        std::fs::write(&original, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(&original, content_dir.join("bin").join("tool-alias")).unwrap();

        assert!(
            std::fs::symlink_metadata(&original).unwrap().nlink() > 1,
            "the fixture must be a genuine in-tree hardlink pair, or this pins nothing",
        );

        let binaries: Binaries = serde_json::from_str(r#"["tool"]"#).unwrap();
        ensure_declared_binaries_executable(&content_dir, &binaries)
            .await
            .unwrap();

        assert_eq!(
            std::fs::metadata(&original).unwrap().permissions().mode() & 0o777,
            0o644,
            "a declared binary that happens to be hardlinked is currently left alone — \
             if this fails, hardlinked archives became extractable and the skip needs replacing \
             with a containment check",
        );
    }

    /// An intermediate path component being a symlink is a whole escape class
    /// this module does not defend against itself: `ocx_util`'s `DirWalker`
    /// classifies with `DirEntry::file_type()`, which does not follow links, so
    /// a symlinked directory is never descended into and its contents are never
    /// scanned. That property lives in another repository and nothing here pins
    /// it — this test fails loudly if `DirWalker` ever gains a follow-symlinks
    /// path, at which point this module needs its own containment check.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_walk_does_not_descend_into_a_symlinked_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().unwrap();
        // A real directory outside the content root, holding a declared name.
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let victim = outside.join("tool");
        std::fs::write(&victim, b"not upstream's to touch").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o600)).unwrap();

        let content_dir = dir.path().join("content");
        std::fs::create_dir(&content_dir).unwrap();
        std::os::unix::fs::symlink("../outside", content_dir.join("vendor")).unwrap();

        let binaries: Binaries = serde_json::from_str(r#"["tool"]"#).unwrap();
        ensure_declared_binaries_executable(&content_dir, &binaries)
            .await
            .expect("a symlinked directory must be skipped, not fail the run");

        assert_eq!(
            std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
            0o600,
            "the walk must not descend through a symlinked directory into files outside the content root",
        );
    }

    #[tokio::test]
    async fn extract_and_bundle_excludes_metadata_from_content() {
        use ocx_util::archive::Archive;

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

    /// The sidecar `pipeline prepare` writes must carry no `platform` key.
    ///
    /// `ocx` retired the field and now rejects a sidecar still carrying it with
    /// a migration error (exit 65), so stamping one would red every test and
    /// push leg in the fleet. Asserted on the raw JSON, since a sidecar that
    /// re-parses proves only that this binary tolerates it.
    #[test]
    fn sidecar_carries_no_platform_key() {
        let dir = tempfile::TempDir::new().unwrap();
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
        let sidecar = sidecar_json(&metadata, &platform).unwrap();

        let value: serde_json::Value = serde_json::from_str(&sidecar).unwrap();
        assert!(
            value.get("platform").is_none(),
            "sidecar must not carry a platform key: {sidecar}"
        );
        // And it is still a sidecar `ocx package push --metadata` can read.
        serde_json::from_str::<AuthoringMetadata>(&sidecar).unwrap();
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

    /// `resolve_metadata` selects `metadata-darwin.json` for `darwin/arm64`
    /// when the platforms map contains that key.
    ///
    /// Pre-fix: if YAML deserialization dropped slash-containing keys, the
    /// map would be empty and this call would always return the default
    /// metadata — the exact bug that caused darwin bundles to embed
    /// `${installPath}/bin` instead of `${installPath}/CMake.app/Contents/bin`.
    #[test]
    fn resolve_metadata_selects_darwin_override_from_yaml_config() {
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
            let metadata = resolve_metadata(&config, platform, dir.path())
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
