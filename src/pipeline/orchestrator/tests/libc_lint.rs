// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── libc_lint: the claim the pipeline never read ──────────────────────
//
// Linux-only, because `PT_INTERP` is what the check reads and the fixture
// is a real compiled binary. Never a hand-written byte array: the subject
// is a binary format, and a fixture the parser never had to face for real
// proves nothing. Same rule ocx's own libc_lint fixtures follow.
#[cfg(target_os = "linux")]
mod libc {
    use super::*;

    /// Stages `bin/tool` as a **dynamically linked** ELF built by the host
    /// C toolchain, bundled where the download would have landed — so
    /// `prepare_offline` skips the fetch and the check meets a real
    /// loader reference.
    ///
    /// A missing `cc` is a hard failure, never a skip: linking this very
    /// test binary already went through a C linker driver, so "cc absent"
    /// is unreachable wherever this runs and a skip would be a green that
    /// never ran.
    async fn stage_dynamic_elf_asset(task_dir: &Path) {
        tokio::fs::create_dir_all(task_dir).await.expect("create task dir");
        let content = task_dir.join("upstream-content");
        std::fs::create_dir_all(content.join("bin")).expect("create fixture tree");

        let source = task_dir.join("tool.c");
        std::fs::write(&source, "int main(void) { return 0; }\n").expect("write fixture source");
        let compiled = std::process::Command::new("cc")
            .arg("-o")
            .arg(content.join("bin").join("tool"))
            .arg(&source)
            .output()
            .expect("cc must be present: linking this test binary already required a C linker driver");
        assert!(
            compiled.status.success(),
            "cc failed to build the fixture binary: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );

        package::bundle(&content, &task_dir.join("asset.tar.xz"), 1)
            .await
            .expect("build fixture asset");
        std::fs::remove_dir_all(&content).expect("drop fixture tree");
    }

    /// The finding this key exists for, and its escape hatch, asserted
    /// against one another on the same fixture.
    ///
    /// The declared platform is a bare `linux/amd64`. Under `os.features`
    /// subset matching that is a positive claim of libc universality, so
    /// publishing a glibc-linked binary under it produces a tile that
    /// resolves onto musl hosts and dies with a bare `No such file or
    /// directory`. Before this the pipeline never read the artifact at all.
    #[tokio::test]
    async fn a_libc_claim_the_binaries_contradict_fails_the_prepare_unless_opted_out() {
        let spec = spec_dir_declaring("");
        let work = tempfile::tempdir().expect("tempdir");

        let refused = work.path().join("checked");
        stage_dynamic_elf_asset(&refused).await;
        let error = prepare_offline(spec.path(), &refused, BinScanMode::Off, true)
            .await
            .expect_err("a glibc binary under a platform declaring no libc must not publish");
        let rendered = format!("{error:#}");

        // Everything an operator staring at a CI log needs to act: which
        // spec (by the target it publishes to), which version, which
        // platform, which file, which libc, and the way through.
        for needle in [
            "mirror/tool",
            "1.0.0",
            "linux/amd64",
            "bin/tool",
            "libc.glibc",
            "libc_lint: false",
        ] {
            assert!(rendered.contains(needle), "message must name {needle}: {rendered}");
        }

        // The other outcome, same fixture, same platform: the opt-out is
        // the only difference, so a green here is the bypass working and
        // not the check having quietly stopped applying.
        let bypassed = work.path().join("bypassed");
        stage_dynamic_elf_asset(&bypassed).await;
        prepare_offline(spec.path(), &bypassed, BinScanMode::Off, false)
            .await
            .expect("libc_lint: false must publish the same tree");
        assert!(
            bypassed.join("bundle.tar.xz").exists(),
            "the bypassed run must leave a publishable bundle"
        );
        assert!(
            !refused.join("bundle.tar.xz").exists(),
            "the refused run must leave nothing publishable behind"
        );
    }

    /// The opt-out silences one check, not the prepare. A `bin_scan:
    /// verify` mismatch on the very same tree still fails — otherwise the
    /// escape hatch for a false libc refusal would quietly become an
    /// escape hatch for everything the tree is checked for.
    #[tokio::test]
    async fn the_opt_out_does_not_suppress_an_unrelated_prepare_failure() {
        let spec = spec_dir_declaring(r#","binaries":["somethingelse"]"#);
        let work = tempfile::tempdir().expect("tempdir");
        let task_dir = work.path().join("task");
        stage_dynamic_elf_asset(&task_dir).await;

        let error = prepare_offline(spec.path(), &task_dir, BinScanMode::Verify, false)
            .await
            .expect_err("an undeclared binary must still fail with the libc check off");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("bin_scan"),
            "the surviving failure must be the bin_scan one: {rendered}"
        );
        assert!(
            !rendered.contains("libc"),
            "and must not be the libc one, which is bypassed: {rendered}"
        );
    }
}
