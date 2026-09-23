// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Fixtures shared by more than one `spec` test module.

/// Helper: base YAML suitable for all §3.1 round-trip tests. Adds the
/// minimum required fields so pipeline-specific blocks can be appended.
pub const MINIMAL_BASE_YAML: &str = r#"
name: shfmt
target:
  registry: ocx.sh
  repository: shfmt
source:
  type: github_release
  owner: mvdan
  repo: sh
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "shfmt_v.*_linux_amd64$"
  linux/arm64:
    - "shfmt_v.*_linux_arm64$"
  darwin/arm64:
    - "shfmt_v.*_darwin_arm64$"
asset_type:
  type: binary
  name: shfmt
"#;
