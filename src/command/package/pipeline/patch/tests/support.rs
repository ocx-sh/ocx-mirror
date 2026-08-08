// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `patch` test module.

use super::super::*;
use ocx_lib::oci::Platform;

pub fn descriptor(media_type: &str) -> Descriptor {
    Descriptor {
        media_type: media_type.to_string(),
        digest: format!("sha256:{}", "a".repeat(64)),
        size: 42,
        urls: None,
        annotations: None,
        artifact_type: None,
    }
}

pub fn image(layers: Vec<Descriptor>) -> PublishedImage {
    PublishedImage {
        version: version("3.29.0"),
        platform: "linux/amd64".parse::<Platform>().expect("valid platform"),
        manifest_digest: ocx_lib::oci::Digest::Sha256("b".repeat(64)),
        config: descriptor("application/vnd.ocx.package.metadata.v1+json"),
        layers,
    }
}

/// The tag list as `list_target_tags` returns it — every test enters
/// through `Selection::apply`, which is the production entry point, so the
/// leaf reduction is under test rather than reproduced here.
pub fn tag_list(tags: &[&str]) -> Vec<String> {
    tags.iter().map(|t| (*t).to_string()).collect()
}

pub fn tar_xz() -> &'static str {
    ArchiveMediaType::TarXz.as_media_type()
}

pub fn version(raw: &str) -> Version {
    Version::parse(raw).unwrap_or_else(|| panic!("'{raw}' is a version"))
}

/// The wire media types, read off the same enum the production code maps
/// from — a literal here would let the two drift and still pass.
pub fn tar_gz() -> &'static str {
    ArchiveMediaType::TarGz.as_media_type()
}

pub fn selected_tags(selection: &Selection, tags: &[&str]) -> Vec<String> {
    selection
        .apply(&tag_list(tags))
        .expect("selection applies")
        .into_values()
        .collect()
}
