// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Turning a spec platform key into the names the rest of the pipeline uses.
//!
//! The slug is the join key between three parties: `pipeline prepare` names
//! the work directory and the bundle with it, the CI renderer names the JUnit
//! file with it, and `pipeline push` looks the result back up by it. All three
//! call these, or the leg reads as missing and nothing is ever published.
//!
//! `container_id` is the same contract one level down, for the per-image leg.

use ocx_lib::oci::Platform;

/// The repository basename of a container image, with the registry prefix and
/// the tag stripped (`docker.io/library/alpine:3.20` → `alpine`).
///
/// Every distro-family inference keys off this one spelling, so a
/// registry-qualified image classifies the same way its bare form does.
pub fn image_basename(image: &str) -> &str {
    // Strip the tag (everything after `:`), then take the last path component.
    let image_name = image.split(':').next().unwrap_or(image);
    image_name.split('/').next_back().unwrap_or(image_name)
}

/// Infer the default shell for a container image based on its image-name prefix.
///
/// Returns `Some(shell)` when a well-known distro prefix matches, `None` when
/// the image is non-standard and an explicit `shell` is required.
pub fn infer_shell_from_image(image: &str) -> Option<&'static str> {
    let base = image_basename(image);

    // Well-known distros that default to bash.
    const BASH_PREFIXES: &[&str] = &["ubuntu", "debian", "fedora", "rocky", "opensuse"];
    // Alpine defaults to sh (no bash by default).
    const SH_PREFIXES: &[&str] = &["alpine"];

    for prefix in BASH_PREFIXES {
        if base.starts_with(prefix) {
            return Some("bash");
        }
    }
    for prefix in SH_PREFIXES {
        if base.starts_with(prefix) {
            return Some("sh");
        }
    }

    None
}

/// The libc family a container image's userland links against, which selects
/// the statically-linked `ocx` release a container test leg mounts.
///
/// Alpine is musl; every other supported base (Debian, Ubuntu, Fedora, Rocky,
/// openSUSE) is gnu. Running a gnu-linked `ocx` on Alpine fails with a bare
/// "not found" from the loader, so this is the difference between a leg that
/// tests the artifact and one that cannot start.
///
// ponytail: name-prefix inference, not a spec field — the corpus needs exactly
// alpine(musl) + the glibc distros. Add an explicit `containers[].libc` to
// `ContainerConfig` when a musl image that is not Alpine shows up.
pub fn infer_libc_from_image(image: &str) -> &'static str {
    if image_basename(image).starts_with("alpine") {
        "musl"
    } else {
        "gnu"
    }
}

/// The `os.features` value that declares a given libc family.
///
/// The rust triple spells glibc `gnu`; the OCI feature spells it `libc.glibc`.
/// Crossing the two names is the whole point of the cross-check, so the
/// translation lives in one place.
///
/// Distinct from [`libc_feature`](wheels::libc_feature), which reads a feature
/// back off a platform key: this one goes the other way, from an inferred
/// family name to the feature that would declare it.
pub fn libc_family_feature(family: &str) -> &'static str {
    if family == "musl" { "libc.musl" } else { "libc.glibc" }
}

/// The on-disk / artifact-name slug for a platform.
///
/// `linux/amd64` → `linux_amd64`; `linux/amd64+libc.musl` → `linux_amd64_libc.musl`.
///
/// This is the second join key of the pipeline (after
/// [`image_to_container_id`]): `pipeline prepare` names its work directory with
/// it, the CI workflow flattens that directory into `bundle-{V}-{slug}.tar.xz`
/// and `junit-{V}-{slug}-{container_id}.xml`, and `pipeline push` looks both
/// back up by it. `ascii_segments` drops `os_features`, so two platforms
/// differing only by libc would collide — the sorted, deduped feature suffix is
/// what keeps them apart. Every producer and consumer must call this one
/// function or a libc-bearing platform's artifacts become invisible downstream.
pub fn platform_slug(platform: &Platform) -> String {
    use ocx_lib::utility::string_ext::StringExt as _;

    let mut slug = platform.ascii_segments().join("_");

    if let Platform::Specific { os_features, .. } = platform
        && !os_features.is_empty()
    {
        let mut sorted = os_features.clone();
        sorted.sort();
        sorted.dedup();
        slug.push('_');
        slug.push_str(&sorted.join("_").to_relaxed_slug());
    }

    slug
}

/// [`platform_slug`] for a spec platform key in string form.
///
/// An unparseable key falls back to the naive `/` → `_` replacement; validation
/// rejects such keys, so the fallback only keeps callers total.
pub fn platform_key_slug(key: &str) -> String {
    key.parse::<Platform>()
        .map(|p| platform_slug(&p))
        .unwrap_or_else(|_| key.replace('/', "_"))
}

/// A spec platform key stripped of its `os.features` suffix.
///
/// `docker run --platform` speaks OCI `os/arch[/variant]` and rejects the
/// `+libc.musl` suffix outright, while the matrix label, the `--platform` flag
/// of `ocx package test` and the `discover` platform set all need the full key.
/// Strip only where docker looks.
pub fn platform_without_features(key: &str) -> String {
    key.parse::<Platform>()
        .map(|p| p.segments().join("/"))
        .unwrap_or_else(|_| key.to_string())
}

/// Slugify a container image reference into a JUnit-filename `container_id`.
///
/// All `:`, `/` and `.` separators become `_`, and consecutive underscores
/// collapse — e.g. `ubuntu:24.04` → `ubuntu_24_04`, `alpine:3.20` → `alpine_3_20`.
///
/// This is the join key between the two halves of a container run: the CI
/// renderer names each leg's JUnit file with it, and `pipeline push` looks the
/// file back up by it. The two must agree exactly or every container leg's
/// result reads as missing and nothing is ever published — so both call this.
pub fn image_to_container_id(image: &str) -> String {
    image.replace([':', '/', '.'], "_").replace("__", "_")
}
