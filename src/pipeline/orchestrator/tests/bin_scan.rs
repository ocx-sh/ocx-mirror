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
    package::bundle(&content, at, 1).await.expect("build fixture asset");
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
    package::bundle(&content, at, 1).await.expect("build fixture asset");
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
    package::bundle(&content, at, 1).await.expect("build fixture asset");
    std::fs::remove_dir_all(&content).expect("drop fixture tree");
}

/// Runs the real prepare phase offline: the asset is staged where the
/// download would have written it, so `prepare_task` skips the fetch and
/// exercises extract → scan → sidecar → bundle exactly as in production.
#[cfg(unix)]
async fn prepare_scanned(spec_dir: &Path, task_dir: &Path, bin_scan: BinScanMode) -> Result<Metadata> {
    prepare_offline(spec_dir, task_dir, bin_scan, true).await
}

/// The names on disk, as the sidecar `ocx package push --metadata` reads
/// records them — the file, not the in-memory value, because that file is
/// what the CI push job actually publishes from.
#[cfg(unix)]
fn sidecar_binaries(task_dir: &Path) -> Vec<String> {
    let json = std::fs::read_to_string(task_dir.join("metadata.json")).expect("sidecar written");
    let sidecar: AuthoringMetadata = serde_json::from_str(&json).expect("sidecar parses");
    sidecar
        .binaries()
        .map(|binaries| binaries.iter().map(|name| name.as_str().to_string()).collect())
        .unwrap_or_default()
}

/// The ordering constraint this feature exists to satisfy: `binaries` is
/// derived from the extracted content tree, so the metadata cannot be
/// finalised before the download the way every other field is.
///
/// Asserted against the sidecar as well as the published projection: the CI
/// push job publishes from the file, so a claim that reached only the
/// in-memory value would never leave the runner.
#[cfg(unix)]
#[tokio::test]
async fn bin_scan_auto_fills_binaries_from_the_extracted_tree() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");

    let off = work.path().join("off");
    let metadata = prepare_scanned(spec.path(), &off, BinScanMode::Off)
        .await
        .expect("prepare succeeds");
    assert!(
        metadata.binaries().is_none(),
        "control: without bin_scan nothing may invent a binaries claim",
    );
    assert!(sidecar_binaries(&off).is_empty(), "control: sidecar too");

    let auto = work.path().join("auto");
    let metadata = prepare_scanned(spec.path(), &auto, BinScanMode::Auto)
        .await
        .expect("prepare succeeds");
    assert_eq!(
        metadata.binaries().map(|binaries| binaries.len()),
        Some(1),
        "the executable under the interface PATH dir must reach the published metadata",
    );
    assert_eq!(sidecar_binaries(&auto), vec!["tool"], "and the sidecar the push reads");
}

/// `verify` is the mode a mirror wants once it hand-lists `binaries`: the
/// list becomes a regression test against upstream rearranging its archive,
/// and a disagreement fails the run instead of publishing quietly.
#[cfg(unix)]
#[tokio::test]
async fn bin_scan_verify_fails_on_an_undeclared_binary() {
    let spec = spec_dir_declaring(r#","binaries":["other"]"#);
    let work = tempfile::tempdir().expect("tempdir");

    let error = prepare_scanned(spec.path(), &work.path().join("verify"), BinScanMode::Verify)
        .await
        .expect_err("an executable the spec does not declare must fail the run");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("tool") && rendered.contains("not declared"),
        "the failure must name the undeclared binary: {rendered}",
    );

    // The same tree under `auto` passes a declared list through untouched —
    // otherwise the test above would prove nothing about `verify`.
    let metadata = prepare_scanned(spec.path(), &work.path().join("auto"), BinScanMode::Auto)
        .await
        .expect("auto passes a declared claim through unverified");
    assert_eq!(metadata.binaries().map(|binaries| binaries.len()), Some(1));
}

/// A resume arrives after the content tree is gone, so re-resolving the
/// metadata from the spec would silently drop the scanned claim and publish
/// a bundle whose metadata contradicts what was prepared beside it.
///
/// The sidecar the first run wrote is the record, and it is written before
/// the bundle exists precisely so this readback cannot miss.
#[cfg(unix)]
#[tokio::test]
async fn a_resumed_bin_scan_task_keeps_the_scanned_binaries() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");

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
}

/// A resume adopts `binaries` from the old sidecar and *nothing else*.
///
/// Carrying the whole sidecar kept the scanned claim but froze every other
/// field with it, so a spec-side fix landing between the two runs — here a
/// second env var — was silently discarded on any resumed run and the
/// corrected metadata never published.
#[cfg(unix)]
#[tokio::test]
async fn a_resumed_bin_scan_task_still_picks_up_a_spec_metadata_fix() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
        .await
        .expect("first run succeeds");

    // The spec-side fix, applied after the bundle exists.
    write_metadata(
        spec.path(),
        "bin",
        "",
        r#",{"key":"TOOL_HOME","type":"constant","value":"${installPath}","required":false,"visibility":"interface"}"#,
    );

    let resumed = prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
        .await
        .expect("resume succeeds");

    assert!(
        resumed
            .env()
            .is_some_and(|vars| vars.into_iter().any(|var| var.key == "TOOL_HOME")),
        "a resumed run must re-resolve everything the spec owns: {:?}",
        resumed
            .env()
            .map(|vars| vars.into_iter().map(|v| v.key.clone()).collect::<Vec<_>>()),
    );
    assert_eq!(
        resumed.binaries().map(|binaries| binaries.len()),
        Some(1),
        "and must still carry the scanned claim it cannot recompute",
    );
}

/// A scan that finds nothing must fail the run, not publish `binaries: []`.
///
/// The load-time gate proves the metadata *declares* an
/// `${installPath}/<dir>` PATH entry; it cannot prove that directory exists
/// in the archive. A typo or an upstream rename yields zero candidates and
/// the same false "exposes no executables" claim the gate exists to stop —
/// and under `verify`, with nothing declared and nothing found, the
/// one-directional diff is trivially empty and it went green.
///
/// Both legs use a fixture that declares **no** `binaries`, which is the
/// only shape reaching this guard: `verify` here is the fill path. For
/// `(verify, declared)` `resolve_binaries` returns the hand-written list
/// untouched and nothing observes the tree at all — that gap is ocx's ADR §2
/// rule that a declared-but-absent name is legal, recorded as a follow-up
/// and deliberately not asserted here.
#[cfg(unix)]
#[tokio::test]
async fn a_scan_target_missing_from_the_archive_fails_instead_of_claiming_nothing() {
    let work = tempfile::tempdir().expect("tempdir");

    for (mode, label) in [(BinScanMode::Auto, "auto"), (BinScanMode::Verify, "verify")] {
        let spec = spec_dir_declaring("");
        // The archive ships `bin/`; the metadata points somewhere else.
        write_metadata(spec.path(), "not-in-the-archive", "", "");

        let error = prepare_scanned(spec.path(), &work.path().join(label), mode)
            .await
            .expect_err("a scan target absent from the archive must fail the run");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("found no executables"),
            "{label}: the failure must say the scan came up empty: {rendered}",
        );
    }
}

/// The resume path runs the same guard as the fresh path.
///
/// A resumed scanning task takes an early return that skipped
/// `reject_empty_scan` entirely, so it could republish `binaries: []` — or,
/// off a sidecar that never carried the field, no claim at all. Either then
/// reads as "nothing to adopt" on the download-free paths, so `plan` reports
/// no drift and the mistake is permanent.
#[cfg(unix)]
#[tokio::test]
async fn a_resumed_scanning_task_rejects_an_unusable_claim() {
    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("resume");

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
/// Asserted against the extracted bundle, not `content/` — prepare drops the
/// tree, and the bundle is what gets pushed.
#[cfg(unix)]
#[tokio::test]
async fn a_declared_binary_shipped_without_an_exec_bit_is_published_executable() {
    use std::os::unix::fs::PermissionsExt;

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

    let published = work.path().join("published");
    ocx_lib::archive::Archive::extract(task_dir.join("bundle.tar.xz"), &published)
        .await
        .expect("the produced bundle extracts");

    let mode = |name: &str| {
        std::fs::metadata(published.join(name))
            .unwrap_or_else(|e| panic!("{name} missing from the bundle: {e}"))
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(
        mode("pwsh"),
        0o755,
        "a declared binary must be executable in the published bundle",
    );
    assert_eq!(
        mode("LICENSE.txt"),
        0o644,
        "and nothing else may be touched — this is not a blanket chmod -R",
    );
}

/// **Limitation pin, not an aspiration.** Under `bin_scan: auto` with no
/// hand-written `binaries`, the #51 chmod cannot reach a non-executable
/// file — and the assertions below record that as the behavior.
///
/// `resolve_binaries` fills the claim from `scan_interface_binaries`, which
/// keeps executable candidates only, so a 0644 file never enters the list
/// the chmod is keyed on: `pwsh` is absent from the published claim *and*
/// still 0644 in the bundle. A mirror hitting #51 fixes it by declaring
/// `binaries` by hand, not by switching scan modes. Rewrite this test only
/// alongside a deliberate decision to let an auto fill claim files it found
/// non-executable.
#[cfg(unix)]
#[tokio::test]
async fn an_auto_scan_with_no_declared_list_leaves_a_non_executable_binary_unfixed() {
    use std::os::unix::fs::PermissionsExt;

    let spec = spec_dir_declaring("");
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("task");
    tokio::fs::create_dir_all(&task_dir).await.expect("create task dir");
    staged_mixed_mode_asset(&task_dir.join("asset.tar.xz")).await;

    prepare_scanned(spec.path(), &task_dir, BinScanMode::Auto)
        .await
        .expect("prepare succeeds");

    assert_eq!(
        sidecar_binaries(&task_dir),
        vec!["tool"],
        "the mechanism: a fill claims only what it found executable",
    );

    let published = work.path().join("published");
    ocx_lib::archive::Archive::extract(task_dir.join("bundle.tar.xz"), &published)
        .await
        .expect("the produced bundle extracts");
    let mode = |relative: &str| {
        std::fs::metadata(published.join(relative))
            .unwrap_or_else(|e| panic!("{relative} missing from the bundle: {e}"))
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode("bin/tool"), 0o755, "control: the claimed binary is executable");
    assert_eq!(
        mode("bin/pwsh"),
        0o644,
        "a name no scan claimed stays as upstream shipped it — declare it to have it chmodded",
    );
}

/// `bin_scan: verify` refuses a declared-but-non-executable binary, and the
/// chmod must not have papered over it first.
///
/// The two features disagree by design: #51's chmod makes a declared name
/// executable, `verify` fails the run for exactly that state. `verify` wins
/// — a mirror that asked to be told when upstream changes its archive must
/// be told — and today that holds only because `resolve_binaries` runs ten
/// lines above the chmod in `prepare_task`. Swapping the two statements
/// passes every other test in this file, so the mode assertion below is the
/// one thing pinning the order.
#[cfg(unix)]
#[tokio::test]
async fn bin_scan_verify_refuses_a_non_executable_binary_before_the_chmod_runs() {
    use std::os::unix::fs::PermissionsExt;

    let spec = spec_dir_declaring(r#","binaries":["pwsh"]"#);
    let work = tempfile::tempdir().expect("tempdir");
    let task_dir = work.path().join("task");
    tokio::fs::create_dir_all(&task_dir).await.expect("create task dir");
    staged_non_executable_bin_dir_asset(&task_dir.join("asset.tar.xz")).await;

    let error = prepare_scanned(spec.path(), &task_dir, BinScanMode::Verify)
        .await
        .expect_err("a declared binary that is not executable must fail a verify run");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("pwsh") && rendered.contains("not executable"),
        "the failure must name the offending binary: {rendered}",
    );

    let extracted = task_dir.join("content").join("bin").join("pwsh");
    assert_eq!(
        std::fs::metadata(&extracted)
            .expect("the extracted tree survives a refused run")
            .permissions()
            .mode()
            & 0o777,
        0o644,
        "the chmod must not run before verify — a fixed-up mode would make the refusal unreachable",
    );
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
