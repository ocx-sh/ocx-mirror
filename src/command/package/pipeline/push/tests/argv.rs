// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use super::super::*;
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
            "--new",
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
