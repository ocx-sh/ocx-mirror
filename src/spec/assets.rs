// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::collections::HashMap;

use ocx_lib::oci::Platform;
use serde::Deserialize;
use serde::de::{self, Deserializer};

/// Asset selection rules mapping platforms to ordered lists of filename regexes.
///
/// Platform keys are `os/arch` format (e.g. `linux/amd64`). Each platform maps to
/// one or more regex patterns that match asset filenames for that platform.
#[derive(Debug, Clone)]
pub struct AssetPatterns {
    pub patterns: HashMap<Platform, Vec<String>>,
}

impl<'de> Deserialize<'de> for AssetPatterns {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: HashMap<String, Vec<String>> = HashMap::deserialize(deserializer)?;
        let mut patterns = HashMap::with_capacity(raw.len());
        for (key, value) in raw {
            let platform: Platform = key
                .parse()
                .map_err(|_| de::Error::custom(format!("invalid platform '{key}'")))?;
            patterns.insert(platform, value);
        }
        Ok(Self { patterns })
    }
}

impl AssetPatterns {
    pub fn validate(&self, errors: &mut Vec<String>) {
        for (platform, patterns) in &self.patterns {
            for pattern in patterns {
                if let Err(e) = regex::Regex::new(pattern) {
                    errors.push(format!("assets.{platform}: invalid regex '{pattern}': {e}"));
                }
            }
        }
    }

    /// Compile pattern strings into regexes, keyed by platform.
    /// Platform validation is done at deserialization time.
    pub fn compiled(&self) -> Result<HashMap<Platform, Vec<regex::Regex>>, String> {
        let mut result = HashMap::new();
        for (platform, patterns) in &self.patterns {
            let mut regexes = Vec::with_capacity(patterns.len());
            for pattern in patterns {
                let re = regex::Regex::new(pattern).map_err(|e| format!("invalid regex '{pattern}': {e}"))?;
                regexes.push(re);
            }
            result.insert(platform.clone(), regexes);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> AssetPatterns {
        serde_yaml_ng::from_str(yaml).expect("valid asset patterns")
    }

    #[test]
    fn libc_key_parses_distinct_platforms() {
        // A `+libc.glibc` suffix on the key flows through Platform::from_str
        // into os_features — no object-form YAML needed.
        let patterns = parse("\"linux/amd64+libc.glibc\":\n  - cpython-.*-gnu\\.tar\\.zst\n");
        let (platform, _) = patterns.patterns.iter().next().expect("one entry");
        match platform {
            Platform::Specific { os_features, .. } => {
                assert_eq!(os_features, &vec!["libc.glibc".to_string()]);
            }
            other => panic!("expected specific platform, got {other:?}"),
        }
    }

    #[test]
    fn glibc_and_musl_keys_coexist() {
        // Same os/arch, different libc are distinct Platform keys — they must
        // not collapse into one HashMap entry.
        let patterns = parse(concat!(
            "\"linux/amd64+libc.glibc\":\n  - cpython-.*-gnu\\.tar\\.zst\n",
            "\"linux/amd64+libc.musl\":\n  - cpython-.*-musl\\.tar\\.zst\n",
        ));
        assert_eq!(patterns.patterns.len(), 2);
    }
}
