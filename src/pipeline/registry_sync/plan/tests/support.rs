// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `plan` test module.

use ocx_lib::oci::Algorithm;

use super::super::*;
use crate::spec::{OnError, RegistryConcurrency, Target};

/// The destination registry and prefix every test mirrors into.
pub const REGISTRY: &str = "registry.test";
pub const PREFIX: &str = "mirror";

/// A spec whose `destination:` uses all three placeholders, so a test that
/// changes the source's `as:` or the catalog key changes the repository.
///
/// `rewrite_pointers` is `true` here, **against the field's own default**, so
/// that every expansion test can assert the pointer it produces. The default
/// (preserve) is exercised by [`preserving`], which flips it back.
pub fn spec(destination: &str) -> RegistrySpec {
    RegistrySpec {
        target: Target {
            registry: REGISTRY.to_string(),
            repository: PREFIX.to_string(),
        },
        output: std::path::PathBuf::from("./public"),
        destination: destination.to_string(),
        rewrite_pointers: true,
        publish_tags: true,
        on_error: OnError::default(),
        sources: Vec::new(),
        concurrency: RegistryConcurrency::default(),
    }
}

/// [`spec`] with `rewrite_pointers` back at its default — the mirrored index
/// keeps whatever pointer the source published.
pub fn preserving(destination: &str) -> RegistrySpec {
    RegistrySpec {
        rewrite_pointers: false,
        ..spec(destination)
    }
}

/// A source with the given filters. `as_name` is threaded to `plan_source`
/// separately — `RegistrySource::as_name` is WP-08's, and this field is only
/// its input.
pub fn source(include: &[&str], exclude: &[&str]) -> RegistrySource {
    RegistrySource {
        registry: "ghcr.io".to_string(),
        index: "https://index.example/".to_string(),
        as_name: None,
        include: include.iter().map(|pattern| (*pattern).to_string()).collect(),
        exclude: exclude.iter().map(|pattern| (*pattern).to_string()).collect(),
        trusted_hosts: Vec::new(),
    }
}

/// A source catalog: `<ns>/<pkg>` → `sha256:<root digest>`. The values never
/// matter to the plan phase — only the key set does.
pub fn catalog(names: &[&str]) -> CatalogIndex {
    names
        .iter()
        .map(|name| ((*name).to_string(), digest(name).to_string()))
        .collect()
}

/// `sha256` of a short unique string.
pub fn digest(seed: &str) -> ocx_lib::oci::Digest {
    Algorithm::Sha256.hash(seed.as_bytes())
}

/// The `(name, physical_repository)` pairs of a work list, which is what every
/// expansion assertion is actually about.
pub fn destinations(work: &[PackageWork]) -> Vec<(String, String)> {
    work.iter()
        .map(|package| (package.name.clone(), package.physical_repository.clone()))
        .collect()
}

/// The names in a work list, in order.
pub fn names(work: &[PackageWork]) -> Vec<String> {
    work.iter().map(|package| package.name.clone()).collect()
}

/// The messages of a plan-time refusal, panicking on any other class — a
/// refusal here happens before a byte is copied and must stay exit 65.
pub fn refusal_messages(error: &MirrorError) -> String {
    match error {
        MirrorError::SpecInvalid(messages) => messages.join("; "),
        other => panic!("expected a plan-time SpecInvalid refusal, got {other:?}"),
    }
}
