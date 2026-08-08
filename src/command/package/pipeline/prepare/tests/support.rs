// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `prepare` test module.

use super::super::*;

/// Inline url_index spec (offline) with a late-introduced `windows/arm64`
/// platform: `min_version: 0.11.7`. Used to verify resolve drops
/// out-of-window `(version, platform)` pairs from the prepare task list.
pub const APPLICABILITY_SPEC: &str = r#"
name: testtool
target:
  registry: ocx.sh
  repository: testtool
source:
  type: url_index
  versions:
    "0.10.0":
      assets:
        tool-linux-amd64: "https://example.com/0.10.0/linux-amd64"
        tool-windows-arm64: "https://example.com/0.10.0/windows-arm64"
    "0.11.8":
      assets:
        tool-linux-amd64: "https://example.com/0.11.8/linux-amd64"
        tool-windows-arm64: "https://example.com/0.11.8/windows-arm64"
    "0.12.0":
      assets:
        tool-linux-amd64: "https://example.com/0.12.0/linux-amd64"
        tool-windows-arm64: "https://example.com/0.12.0/windows-arm64"
assets:
  linux/amd64:
    - "tool-linux-amd64$"
  windows/arm64:
    - "tool-windows-arm64$"
asset_type:
  type: binary
  name: tool
build_timestamp: none
platforms:
  linux/amd64:
    runner: ubuntu-latest
  windows/arm64:
    runner: windows-11-arm
    min_version: "0.11.7"
    exclude:
      - version: "0.12.0"
        reason: "broken on this release"
"#;

/// A stand-in interpreter candidate set with a fixed digest, so
/// `build_env_tasks` runs without resolving a real registry manifest.
/// The single `any`-platform candidate is compatible with every leg.
pub fn fake_interpreter_candidates() -> Vec<(ocx_lib::oci::Identifier, ocx_lib::oci::Platform)> {
    let reference = format!("ocx.sh/cpython:3.13@sha256:{}", "a".repeat(64));
    let identifier = ocx_lib::oci::Identifier::parse(&reference).expect("interpreter reference parses");
    vec![(identifier, ocx_lib::oci::Platform::Any)]
}

pub fn platforms_of(tasks: &[MirrorTask]) -> Vec<String> {
    let mut p: Vec<String> = tasks.iter().map(|t| t.platform.to_string()).collect();
    p.sort();
    p
}
