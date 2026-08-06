// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

use std::fmt;

use serde::Deserialize;
use serde::de;

/// Rolling-tag cascade settings.
///
/// Spelled either as a bool — `cascade: true` / `cascade: false` — or as a map
/// that opts the generated `cascade.yml` into a `schedule:` trigger:
///
/// ```yaml
/// cascade:
///   schedule: "17 4 * * 1"
/// ```
///
/// The map form always means enabled; `schedule:` inside it is optional, so a
/// bare `cascade: {}` is the same as `cascade: true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeConfig {
    /// Whether this spec publishes rolling cascade tags at all.
    pub enabled: bool,

    /// Cron expression for the generated `cascade.yml`'s `schedule:` trigger.
    /// Absent → the workflow is dispatch-only. Charset-checked by
    /// [`CascadeConfig::validate`]; GitHub validates the semantics.
    pub schedule: Option<String>,
}

impl Default for CascadeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            schedule: None,
        }
    }
}

impl CascadeConfig {
    pub(crate) fn validate(&self, errors: &mut Vec<String>) {
        if let Some(cron) = &self.schedule {
            super::validate_cron("cascade.schedule", cron, errors);
        }
    }
}

/// The map spelling of `cascade:`.
///
/// Deserialized on its own rather than as an `#[serde(untagged)]` variant: an
/// untagged enum reports every failure as "data did not match any variant",
/// swallowing the `unknown field` diagnostic that tells an operator which key
/// they misspelled.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CascadeMap {
    #[serde(default)]
    schedule: Option<String>,
}

impl<'de> Deserialize<'de> for CascadeConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(CascadeVisitor)
    }
}

struct CascadeVisitor;

impl<'de> de::Visitor<'de> for CascadeVisitor {
    type Value = CascadeConfig;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a boolean, or a map with an optional `schedule` key")
    }

    fn visit_bool<E: de::Error>(self, enabled: bool) -> Result<Self::Value, E> {
        Ok(CascadeConfig {
            enabled,
            schedule: None,
        })
    }

    fn visit_map<M: de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        let map = CascadeMap::deserialize(de::value::MapAccessDeserializer::new(map))?;
        Ok(CascadeConfig {
            enabled: true,
            schedule: map.schedule,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> CascadeConfig {
        serde_yaml_ng::from_str(yaml).expect("cascade value must parse")
    }

    fn parse_err(yaml: &str) -> String {
        serde_yaml_ng::from_str::<CascadeConfig>(yaml)
            .expect_err("cascade value must be rejected")
            .to_string()
    }

    fn errors_for(schedule: &str) -> Vec<String> {
        let mut errors = Vec::new();
        CascadeConfig {
            enabled: true,
            schedule: Some(schedule.to_string()),
        }
        .validate(&mut errors);
        errors
    }

    #[test]
    fn a_bool_keeps_its_original_meaning() {
        assert_eq!(parse("true"), CascadeConfig::default());
        assert_eq!(
            parse("false"),
            CascadeConfig {
                enabled: false,
                schedule: None
            }
        );
    }

    #[test]
    fn a_map_implies_enabled_and_carries_the_schedule() {
        assert_eq!(
            parse("schedule: '17 4 * * 1'"),
            CascadeConfig {
                enabled: true,
                schedule: Some("17 4 * * 1".to_string()),
            }
        );
        // No schedule inside the map is the bool `true` again — an operator
        // spelling out the map before picking a cron must not disable repair.
        assert_eq!(parse("{}"), CascadeConfig::default());
    }

    #[test]
    fn a_misspelled_key_is_rejected_by_name() {
        // A typo that parsed would silently render a dispatch-only workflow, so
        // the message has to name the key the operator must go fix.
        let err = parse_err("scedule: '0 4 * * 1'");
        assert!(err.contains("unknown field `scedule`"), "{err}");
        assert!(err.contains("`schedule`"), "{err}");
    }

    #[test]
    fn a_non_bool_scalar_names_both_accepted_shapes() {
        // `yes` is a YAML 1.1 bool but a 1.2 string — a plausible spelling whose
        // rejection must not read as "this key wanted a map".
        let err = parse_err("yes");
        assert!(err.contains("invalid type: string \"yes\""), "{err}");
        assert!(
            err.contains("a boolean, or a map with an optional `schedule` key"),
            "{err}"
        );
    }

    #[test]
    fn a_schedule_that_could_reshape_the_on_block_is_rejected() {
        // The value is spliced into `on:` inside a single-quoted scalar; a quote
        // or a newline closes it and adds triggers of the spec's choosing — and
        // a non-schedule trigger runs the repair for real, unattended.
        assert!(!errors_for("0 4 * * 1'\n  push:\n    branches: [main]\n#").is_empty());
        // An empty cron renders `- cron: ''`, which GitHub rejects wholesale —
        // taking the dispatch trigger down with it.
        assert!(!errors_for("").is_empty());
        assert!(!errors_for("   ").is_empty());

        assert!(errors_for("17 4 * * 1").is_empty());
        assert!(errors_for("0 */6 * * MON-FRI").is_empty());
    }
}
