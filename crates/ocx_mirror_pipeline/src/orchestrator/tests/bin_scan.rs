// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
use super::support::*;

// ── bin_scan: the claim that only exists after extraction ─────────────

/// A `.tar.xz` whose declared interface binary sits at the archive **root**
/// at 0644, beside an undeclared file at the same mode — the shape issue #51
/// reproduces: upstream ships `pwsh` non-executable and the metadata's PATH
/// is the bare `${installPath}`, so no scan can be pointed at it.
#[cfg(unix)]
async fn staged_non_executable_asset(at: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let content = at.parent().expect("asset has a parent").join("upstream-content");
    std::fs::create_dir_all(&content).expect("create fixture tree");
    for name in ["pwsh", "LICENSE.txt"] {
        let file = content.join(name);
        std::fs::write(&file, b"#!/bin/sh\n").expect("write fixture file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod fixture file");
    }
    archive(&content, at).await;
    std::fs::remove_dir_all(&content).expect("drop fixture tree");
}

/// A `.tar.xz` whose `bin/` holds one executable and one non-executable
/// file — the mixed shape a scan pointed at `${installPath}/bin` sees when
/// upstream ships part of its interface without the exec bit.
#[cfg(unix)]
async fn staged_mixed_mode_asset(at: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let content = at.parent().expect("asset has a parent").join("upstream-content");
    std::fs::create_dir_all(content.join("bin")).expect("create fixture tree");
    for (name, mode) in [("tool", 0o755), ("pwsh", 0o644)] {
        let file = content.join("bin").join(name);
        std::fs::write(&file, b"#!/bin/sh\n").expect("write fixture file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).expect("chmod fixture file");
    }
    archive(&content, at).await;
    std::fs::remove_dir_all(&content).expect("drop fixture tree");
}

/// A `.tar.xz` whose `bin/` holds a single non-executable file — nothing a
/// scan would claim, so a `verify` run sees only the declared-but-not-
/// executable disagreement.
#[cfg(unix)]
async fn staged_non_executable_bin_dir_asset(at: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let content = at.parent().expect("asset has a parent").join("upstream-content");
    std::fs::create_dir_all(content.join("bin")).expect("create fixture tree");
    let file = content.join("bin").join("pwsh");
    std::fs::write(&file, b"#!/bin/sh\n").expect("write fixture file");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("chmod fixture file");
    archive(&content, at).await;
    std::fs::remove_dir_all(&content).expect("drop fixture tree");
}

/// Runs the prepare phase offline: the asset is staged where the download
/// would have written it, so `prepare_task` skips the fetch and runs extract →
/// chmod → create → sidecar exactly as in production, against the fake create.
#[cfg(unix)]
async fn prepare_scanned(spec_dir: &Path, task_dir: &Path, bin_scan: BinScanMode) -> Result<Metadata> {
    prepare_offline(spec_dir, task_dir, bin_scan, true).await
}

/// The names the sidecar `ocx package push --metadata` reads records — the
/// file, not the in-memory value, because that file is what the CI push job
/// actually publishes from.
#[cfg(unix)]
fn sidecar_binaries(task_dir: &Path) -> Vec<String> {
    let json = std::fs::read_to_string(task_dir.join("metadata.json")).expect("sidecar written");
    let sidecar: Metadata = serde_json::from_str(&json).expect("sidecar parses");
    sidecar
        .binaries()
        .map(|binaries| binaries.iter().map(|name| name.as_str().to_string()).collect())
        .unwrap_or_default()
}

/// What create compiled is what the run publishes: the returned value (the
/// in-process push) and `metadata.json` (the CI push) both carry the claim
/// create filled, not the spec's authoring input.
///
/// And `bin_scan` reaches create as its own flag: `off` must say so, because
/// create's default is to scan and fill.
#[cfg(unix)]
#[tokio::test]
async fn prepare_publishes_the_sidecar_create_compiled() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");

    let off = work.path().join("off");
    let metadata = prepare_scanned(spec.path(), &off, BinScanMode::Off)
        .await
        .expect("prepare succeeds");
    assert!(metadata.binaries().is_none(), "control: nothing compiled a claim");
    assert!(sidecar_binaries(&off).is_empty(), "control: sidecar too");
    assert!(
        create_invocations(&off)[0].ends_with("--no-bin-scan"),
        "off must switch create's default scan off: {:?}",
        create_invocations(&off),
    );

    let auto = work.path().join("auto");
    stage_compiled_binaries(&auto, spec.path(), &["tool"]);
    let metadata = prepare_scanned(spec.path(), &auto, BinScanMode::Auto)
        .await
        .expect("prepare succeeds");
    assert_eq!(
        metadata.binaries().map(|binaries| binaries.len()),
        Some(1),
        "the compiled claim must reach the published metadata",
    );
    assert_eq!(sidecar_binaries(&auto), vec!["tool"], "and the sidecar the push reads");
    assert!(
        !create_invocations(&auto)[0].contains("bin-scan"),
        "auto is create's default and passes no flag: {:?}",
        create_invocations(&auto),
    );
}

/// A create refusal — an unresolvable dependency tag, a `binaries` mismatch, a
/// libc claim the binaries contradict — fails the prepare, carries create's
/// own explanation, and names the tile an operator has to go and fix.
#[cfg(unix)]
#[tokio::test]
async fn a_create_refusal_fails_the_prepare_naming_the_tile() {
    let spec = spec_dir_declaring(r#","binaries":["other"]"#);
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("verify");
    std::fs::create_dir_all(&task_dir).expect("create task dir");
    std::fs::write(task_dir.join("fake-create-stderr"), "binary 'tool' is not declared").expect("stage refusal");

    let error = prepare_scanned(spec.path(), &task_dir, BinScanMode::Verify)
        .await
        .expect_err("a refused create must fail the run");
    let rendered = format!("{error:#}");
    for needle in ["tool' is not declared", "mirror/tool", "1.0.0", "linux/amd64"] {
        assert!(rendered.contains(needle), "the failure must name {needle}: {rendered}");
    }
    assert!(
        !task_dir.join("metadata.json").exists(),
        "a refused run must leave nothing publishable behind",
    );
    assert!(
        create_invocations(&task_dir)[0].ends_with("--bin-scan"),
        "verify is create's --bin-scan: {:?}",
        create_invocations(&task_dir),
    );
}

/// A resume arrives after the content tree is gone, so nothing can recompile
/// the metadata. The compiled sidecar the first run left is the record, and it
/// is reused without spawning create again.
#[cfg(unix)]
#[tokio::test]
async fn a_resumed_bin_scan_task_keeps_the_scanned_binaries() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");
    stage_compiled_binaries(&task_dir, spec.path(), &["tool"]);

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
        .await
        .expect("first run succeeds");
    assert!(task_dir.join("bundle.tar.xz").exists(), "first run must leave a bundle");
    assert!(
        !task_dir.join("content").exists(),
        "and must have discarded the tree a re-scan would need",
    );

    let resumed = prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
        .await
        .expect("resume succeeds");
    assert_eq!(
        resumed.binaries().map(|binaries| binaries.len()),
        Some(1),
        "a resumed run must republish the scanned claim, not drop it",
    );
    assert_eq!(sidecar_binaries(&task_dir), vec!["tool"]);
    assert_eq!(
        create_invocations(&task_dir).len(),
        1,
        "a resume must not re-create what it cannot recompile",
    );
}

/// A bundle with no sidecar beside it is a create interrupted between the two
/// writes. It is not evidence of anything, so the task is created again rather
/// than refused or trusted.
#[cfg(unix)]
#[tokio::test]
async fn a_bundle_without_its_sidecar_is_created_again() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Off)
        .await
        .expect("first run succeeds");
    std::fs::remove_file(task_dir.join("metadata.json")).expect("drop the sidecar");

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Off)
        .await
        .expect("the second run re-creates");
    assert_eq!(create_invocations(&task_dir).len(), 2, "create must run again");
    assert!(task_dir.join("metadata.json").exists(), "and the sidecar is back");
}

/// The TOOL_HOME env var a spec-side fix adds between two runs.
#[cfg(unix)]
const TOOL_HOME: &str =
    r#",{"key":"TOOL_HOME","type":"constant","value":"${installPath}","required":false,"visibility":"interface"}"#;

/// A resume is keyed on the authoring input create compiled from, not on the
/// bundle existing.
///
/// `package sync` keeps its work dir across a failed push, so reusing the
/// compiled sidecar verbatim silently discarded a spec-side fix landing
/// between the two runs — here a second env var — and the corrected metadata
/// never published.
#[cfg(unix)]
#[tokio::test]
async fn a_resumed_task_still_picks_up_a_spec_metadata_fix() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Off)
        .await
        .expect("first run succeeds");
    write_metadata(spec.path(), "bin", "", TOOL_HOME);

    // As-is: the archive the first run downloaded is still there, and the
    // re-create must use it rather than fetch.
    let task = offline_task(spec.path(), BinScanMode::Off, true);
    let resumed = prepare_as_is(&task, &task_dir).await.expect("resume succeeds");

    assert!(
        resumed
            .env()
            .is_some_and(|vars| vars.into_iter().any(|var| var.key == "TOOL_HOME")),
        "a resumed run must publish the spec's current metadata",
    );
    assert_eq!(
        create_invocations(&task_dir).len(),
        2,
        "a changed spec is created again"
    );
}

/// A re-create that fails must not leave the old sidecar beside the new
/// authoring input: the next run would read the pair as current and publish
/// the metadata the spec fix was meant to replace.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_re_create_leaves_no_sidecar_to_adopt() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Off)
        .await
        .expect("first run succeeds");
    write_metadata(spec.path(), "bin", "", TOOL_HOME);

    std::fs::write(task_dir.join("fake-create-stderr"), "registry unreachable").expect("stage the refusal");
    prepare_scanned(spec.path(), &task_dir, BinScanMode::Off)
        .await
        .expect_err("the re-create fails");
    std::fs::remove_file(task_dir.join("fake-create-stderr")).expect("clear the refusal");

    let resumed = prepare_scanned(spec.path(), &task_dir, BinScanMode::Off)
        .await
        .expect("the third run re-creates");
    assert!(
        resumed
            .env()
            .is_some_and(|vars| vars.into_iter().any(|var| var.key == "TOOL_HOME")),
        "the stale sidecar must not be adopted",
    );
    assert_eq!(create_invocations(&task_dir).len(), 3);
}

/// A scan that finds nothing must fail the run, not publish `binaries: []`.
///
/// The load-time gate proves the metadata *declares* an
/// `${installPath}/<dir>` PATH entry; it cannot prove that directory exists
/// in the archive. A typo or an upstream rename yields zero candidates and
/// the same false "exposes no executables" claim the gate exists to stop —
/// and under `verify`, with nothing declared and nothing found, the
/// one-directional diff is trivially empty and create goes green. So the
/// mirror checks what create compiled.
#[cfg(unix)]
#[tokio::test]
async fn a_scan_target_missing_from_the_archive_fails_instead_of_claiming_nothing() {
    let work = tempfile::tempdir().expect("tempdir");

    for (mode, label) in [(BinScanMode::Auto, "auto"), (BinScanMode::Verify, "verify")] {
        let spec = spec_dir_declaring("");
        // The archive ships `bin/`; the metadata points somewhere else, and
        // create compiles the empty fill that yields.
        write_metadata(spec.path(), "not-in-the-archive", "", "");
        let task_dir = work.path().join(label);
        stage_compiled_binaries(&task_dir, spec.path(), &[]);

        let error = prepare_scanned(spec.path(), &task_dir, mode)
            .await
            .expect_err("a scan target absent from the archive must fail the run");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("found no executables"),
            "{label}: the failure must say the scan came up empty: {rendered}",
        );
        assert!(
            !task_dir.join("metadata.json").exists(),
            "{label}: and must not leave the sidecar under the name CI and resume read",
        );
    }
}

/// The resume path runs the same guard as the fresh path.
///
/// A resumed scanning task takes an early return, so without the guard it
/// could republish `binaries: []` — or, off a sidecar that never carried the
/// field, no claim at all. Either then reads as "nothing to adopt" on the
/// download-free paths, so `plan` reports no drift and the mistake is
/// permanent.
#[cfg(unix)]
#[tokio::test]
async fn a_resumed_scanning_task_rejects_an_unusable_claim() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");
    stage_compiled_binaries(&task_dir, spec.path(), &["tool"]);

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
        .await
        .expect("first run succeeds");

    // Two shapes a sidecar can carry that must not be republished: an empty
    // claim, and no claim at all — an absent field then reads as "nothing to
    // adopt" on the download-free paths, so it is just as permanent. The
    // bundle stays, so each call takes the resume path.
    let sidecar = task_dir.join("metadata.json");
    let original = std::fs::read_to_string(&sidecar).expect("sidecar exists");
    let without_binaries = {
        let mut doc: serde_json::Value = serde_json::from_str(&original).expect("sidecar parses");
        doc.as_object_mut().expect("sidecar is an object").remove("binaries");
        serde_json::to_string(&doc).expect("re-serializes")
    };

    for (label, rewritten) in [
        ("empty claim", original.replace(r#""tool""#, "")),
        ("absent claim", without_binaries),
    ] {
        assert_ne!(
            rewritten, original,
            "{label}: the fixture must actually change the sidecar, or this proves nothing",
        );
        std::fs::write(&sidecar, &rewritten).expect("rewrite sidecar");

        let error = prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
            .await
            .expect_err("a resumed run must not republish an unusable claim");
        assert!(
            format!("{error:#}").contains("found no executables"),
            "{label}: got {error:#}",
        );
    }
}

/// Issue #51: a tar member keeps the mode upstream shipped it with, and
/// PowerShell ships `pwsh` at 0644. The published bundle then installs a
/// command that cannot be run, while the `asset_type: binary` path has
/// always chmodded 0755 — the same package, mirrored two ways, differing in
/// whether its commands work.
///
/// The spec here is the bug's own shape: PATH is the bare `${installPath}`,
/// so there is no `bin/` a scan could be pointed at, and the declared
/// `binaries` list is the only statement of which files are commands.
/// Asserted against the tree create was handed — that tree is what it bundles.
#[cfg(unix)]
#[tokio::test]
async fn a_declared_binary_shipped_without_an_exec_bit_is_published_executable() {
    let spec = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        spec.path().join("metadata.json"),
        r#"{"type":"bundle","version":1,"binaries":["pwsh"],
            "env":[{"key":"PATH","type":"path","value":"${installPath}","required":false,"visibility":"interface"}]}"#,
    )
    .expect("write metadata fixture");

    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("task");
    tokio::fs::create_dir_all(&task_dir).await.expect("create task dir");
    staged_non_executable_asset(&task_dir.join("asset.tar.xz")).await;

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Off)
        .await
        .expect("prepare succeeds");

    assert_eq!(
        mode_create_saw(&task_dir, "pwsh"),
        0o755,
        "a declared binary must be executable in the tree create bundles",
    );
    assert_eq!(
        mode_create_saw(&task_dir, "LICENSE.txt"),
        0o644,
        "and nothing else may be touched — this is not a blanket chmod -R",
    );
}

/// **Limitation pin, not an aspiration.** Under `bin_scan: auto` with no
/// hand-written `binaries`, the #51 chmod cannot reach a non-executable
/// file — and the assertion below records that as the behavior.
///
/// The chmod is keyed on the declared list, and create fills a scanned one
/// only after it has run — keeping executable candidates only, so a 0644 file
/// never enters the claim either. A mirror hitting #51 fixes it by declaring
/// `binaries` by hand, not by switching scan modes. Rewrite this test only
/// alongside a deliberate decision to let an auto fill claim files it found
/// non-executable.
#[cfg(unix)]
#[tokio::test]
async fn an_auto_scan_with_no_declared_list_leaves_a_non_executable_binary_unfixed() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("task");
    tokio::fs::create_dir_all(&task_dir).await.expect("create task dir");
    staged_mixed_mode_asset(&task_dir.join("asset.tar.xz")).await;
    stage_compiled_binaries(&task_dir, spec.path(), &["tool"]);

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
        .await
        .expect("prepare succeeds");

    assert_eq!(
        mode_create_saw(&task_dir, "bin/tool"),
        0o755,
        "control: upstream's own mode"
    );
    assert_eq!(
        mode_create_saw(&task_dir, "bin/pwsh"),
        0o644,
        "a name nothing declared stays as upstream shipped it — declare it to have it chmodded",
    );
}

/// `bin_scan: verify` must get to refuse a declared-but-non-executable binary,
/// so the chmod must not paper over it first.
///
/// The two features disagree by design: #51's chmod makes a declared name
/// executable, `verify` fails the run for exactly that state. `verify` wins —
/// a mirror that asked to be told when upstream changes its archive must be
/// told — and the refusal is create's, so the mirror has to hand create the
/// tree untouched. Under `off` the same fixture is fixed up, which is what
/// makes the `verify` leg an observation rather than an absence.
#[cfg(unix)]
#[tokio::test]
async fn bin_scan_verify_refuses_a_non_executable_binary_before_the_chmod_runs() {
    let spec = spec_dir_declaring(r#","binaries":["pwsh"]"#);
    let work = tempfile::tempdir().expect("tempdir");

    for (mode, expected) in [(BinScanMode::Verify, 0o644), (BinScanMode::Off, 0o755)] {
        let task_dir = work.path().join(format!("{mode:?}"));
        tokio::fs::create_dir_all(&task_dir).await.expect("create task dir");
        staged_non_executable_bin_dir_asset(&task_dir.join("asset.tar.xz")).await;
        stage_compiled_binaries(&task_dir, spec.path(), &["pwsh"]);

        prepare_scanned(spec.path(), &task_dir, mode)
            .await
            .expect("the fake create accepts either tree");
        assert_eq!(
            mode_create_saw(&task_dir, "bin/pwsh"),
            expected,
            "{mode:?}: the chmod must not run before create's verify — a fixed-up mode makes the refusal unreachable",
        );
    }
}

/// A *named* default variant publishes bare tags beside its prefixed ones,
/// and those bare tags must still resolve to a metadata plan.
///
/// `pgo.lto-3.13.9` and `3.13.9` are the same variant's output, but the bare
/// alias carries no variant name — so matching variants by name alone
/// returned `None`, and drift detection and `pipeline patch` skipped every
/// bare tag of such a mirror in silence.
#[test]
fn a_named_default_variants_bare_tags_resolve_to_its_metadata_plan() {
    let yaml = r#"
name: python
target:
  registry: ocx.sh
  repository: python
source:
  type: github_release
  owner: astral-sh
  repo: python-build-standalone
  tag_pattern: "^(?P<version>\\d+)$"
metadata:
  default: metadata.json
variants:
  - name: pgo.lto
    default: true
    bin_scan: verify
    assets:
      linux/amd64: ["pgo-.*\\.tar\\.gz"]
  - name: slim
    assets:
      linux/amd64: ["slim-.*\\.tar\\.gz"]
"#;
    let spec: MirrorSpec = serde_yaml_ng::from_str(yaml).expect("spec parses");
    let plan_for = |tag: &str| metadata_plan_for(&spec, &Version::parse(tag).expect("valid version"));

    let bare = plan_for("3.13.9").expect("a named default's bare tag must resolve to the default variant");
    assert_eq!(
        bare.bin_scan,
        BinScanMode::Verify,
        "and to *that* variant's settings, not the spec-level defaults",
    );
    assert_eq!(
        plan_for("pgo.lto-3.13.9").expect("the prefixed tag resolves").bin_scan,
        BinScanMode::Verify
    );
    assert_eq!(
        plan_for("slim-3.13.9")
            .expect("a non-default variant resolves")
            .bin_scan,
        BinScanMode::Off
    );
    assert!(
        plan_for("gone-3.13.9").is_none(),
        "a tag naming a variant the spec no longer declares must still resolve to nothing",
    );
}

#[test]
fn task_dir_distinguishes_libc_variants() {
    let work = Path::new("/work");
    let glibc = task_dir(work, "3.12.5", &platform("linux/amd64+libc.glibc"));
    let musl = task_dir(work, "3.12.5", &platform("linux/amd64+libc.musl"));

    // Same os/arch, different libc must not collide in one work directory.
    assert_ne!(glibc, musl);
    assert_eq!(glibc, Path::new("/work/3.12.5/linux_amd64_libc.glibc"));
    assert_eq!(musl, Path::new("/work/3.12.5/linux_amd64_libc.musl"));

    // Bare os/arch (no os_features) keeps its plain slug.
    assert_eq!(
        task_dir(work, "3.12.5", &platform("linux/amd64")),
        Path::new("/work/3.12.5/linux_amd64")
    );
}
