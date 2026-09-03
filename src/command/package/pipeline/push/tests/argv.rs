// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::BTreeMap;

use crate::pipeline::ocx_cli::push::build_push_args;

// ── `ocx package push` argv assembly ──────────────────────────────────

#[test]
fn build_push_args_orders_flags_then_bundle_then_annotations() {
    let annotations = BTreeMap::from([
        (
            "org.opencontainers.image.source".to_string(),
            "https://github.com/ocx-sh/mirror-shfmt".to_string(),
        ),
        ("org.opencontainers.image.revision".to_string(), "a1b2c3d4".to_string()),
    ]);

    let args = build_push_args(
        "linux/amd64",
        "ghcr.io/ocx-sh/shfmt:3.8.0",
        &["/bundles/shfmt.tar.xz"],
        None,
        &annotations,
        true,
        None,
    )
    .expect("utf-8 bundle path");

    assert_eq!(
        args,
        vec![
            "--format",
            "json",
            "package",
            "push",
            "--cascade",
            "-p",
            "linux/amd64",
            "-i",
            "ghcr.io/ocx-sh/shfmt:3.8.0",
            "/bundles/shfmt.tar.xz",
            "--annotation",
            "org.opencontainers.image.revision=a1b2c3d4",
            "--annotation",
            "org.opencontainers.image.source=https://github.com/ocx-sh/mirror-shfmt",
        ]
    );
}

/// The C-052 tail is appended to every push leg's argv, and it is last and
/// contiguous.
///
/// Both halves matter. That it is *there* is what makes the archive leg sign
/// at all; that it is one contiguous run at the end is what lets this
/// assertion — and the operator reading a failed job's log — see the whole
/// signing decision in one place rather than interleaved with layer
/// positionals.
#[test]
fn the_sign_tail_is_appended_last_and_whole() {
    use crate::pipeline::ocx_cli::sign::resolve_sign;
    use crate::spec::{KeylessConfig, SignConfig};

    let config = SignConfig {
        keyless: Some(KeylessConfig {
            fulcio: None,
            rekor: None,
            identity_token: None,
        }),
        key: None,
    };
    let sign = resolve_sign(&config, &|_| None, &|_| {
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no files"))
    })
    .expect("public-Sigstore defaults resolve without reading anything");

    let annotations = BTreeMap::from([("org.opencontainers.image.revision".to_string(), "a1b2c3d4".to_string())]);
    let args = build_push_args(
        "linux/amd64",
        "ghcr.io/ocx-sh/shfmt:3.8.0",
        &["/bundles/shfmt.tar.xz"],
        None,
        &annotations,
        true,
        Some(&sign),
    )
    .expect("utf-8 bundle path");

    assert_eq!(
        &args[args.len() - 5..],
        [
            "--sign",
            "--fulcio-url",
            "https://fulcio.sigstore.dev",
            "--rekor-url",
            "https://rekor.sigstore.dev",
        ],
    );
    // The annotation tail is still whole, immediately before it: an argv that
    // interleaved the two would still parse and would be unreadable in a log.
    assert_eq!(args[args.len() - 7], "--annotation");

    // And an unsigned mirror's argv is byte-identical to what it was before
    // signing existed — the property that keeps every other push test honest.
    let unsigned = build_push_args(
        "linux/amd64",
        "ghcr.io/ocx-sh/shfmt:3.8.0",
        &["/bundles/shfmt.tar.xz"],
        None,
        &annotations,
        true,
        None,
    )
    .expect("utf-8 bundle path");
    assert_eq!(unsigned, args[..args.len() - 5]);
}
