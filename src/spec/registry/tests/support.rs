// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Shared fixtures for the `RegistrySpec` tests.

use std::path::{Path, PathBuf};

use super::super::*;

/// The spec body every test here diverges from by exactly one field.
///
/// It carries **no** `kind:` — [`RegistrySpec`] has no such field (C-001), so
/// a document handed straight to serde must not supply one. Loader tests
/// prepend [`KIND_LINE`], which is the form an operator actually writes and
/// which `load_registry_spec` reads and strips.
///
/// The `index:` is `https` and `trusted_hosts` is absent on purpose: this is
/// the body every "one field differs" test starts from, so it must satisfy the
/// transport rule (C-006) without leaning on the exemption. The plaintext half
/// is exercised where it belongs — `validate::a_plaintext_index_*`.
pub const VALID_BODY: &str = r#"
target:
  registry: localhost:5002
  repository: mirror
output: public
destination: "{registry}/{namespace}/{package}"
sources:
  - registry: localhost:5001
    index: https://index.example/
    as: upstream
"#;

/// The `kind:` discriminator, read by the pre-scan and stripped by the loader.
pub const KIND_LINE: &str = "kind: registry\n";

/// [`VALID_BODY`] as an operator writes it — with the discriminator.
pub fn valid_registry_yaml() -> String {
    format!("{KIND_LINE}{VALID_BODY}")
}

/// Deserialize a spec that is expected to parse.
pub fn parse(yaml: &str) -> RegistrySpec {
    serde_yaml_ng::from_str(yaml).unwrap_or_else(|error| panic!("this document must parse: {error}\n{yaml}"))
}

/// The serde error message a document that must **not** parse produces.
pub fn parse_error(yaml: &str) -> String {
    match serde_yaml_ng::from_str::<RegistrySpec>(yaml) {
        Ok(_) => panic!("this document must not parse:\n{yaml}"),
        Err(error) => error.to_string(),
    }
}

/// Validate `yaml` and return the messages, for a document that parses.
pub fn validate(yaml: &str) -> Vec<String> {
    parse(yaml).validate(Path::new("registry.yml"))
}

/// Write `body` to `dir/name` and return the path.
pub fn write_spec(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("the fixture directory is writable");
    path
}
