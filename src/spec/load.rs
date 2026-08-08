// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Reading a spec off disk, resolving its `extends:` chain, and merging.
//!
//! The merge is shallow and child-wins: a key the child sets replaces the
//! base's value outright rather than merging into it, so a child that names
//! `cascade:` owns the whole block.

use std::collections::HashSet;
use std::path::Path;

use ocx_lib::log;

use super::MirrorSpec;
use super::validate::policy_check_notify;
use crate::error::MirrorError;

/// Load and validate a mirror spec from a YAML file, resolving `extends` chains.
///
/// If the spec contains an `extends` key, the referenced base file is loaded first
/// and the child's top-level keys are shallow-merged on top. Chains of arbitrary
/// depth are supported; circular references are detected and rejected.
pub async fn load_spec(spec_path: &Path) -> Result<MirrorSpec, MirrorError> {
    if !spec_path.exists() {
        return Err(MirrorError::SpecNotFound(spec_path.display().to_string()));
    }

    let content = tokio::fs::read_to_string(spec_path)
        .await
        .map_err(|e| MirrorError::SpecNotFound(format!("{}: {e}", spec_path.display())))?;

    let chain = resolve_extends_chain(spec_path, &content).await?;

    let merged = if chain.is_empty() {
        // No extends — parse directly
        serde_yaml_ng::from_str::<serde_yaml_ng::Value>(&content)
            .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error: {e}")]))?
    } else {
        // Load chain in reverse (grandparent first), shallow-merge each layer on top
        let mut base = serde_yaml_ng::Value::Mapping(serde_yaml_ng::Mapping::new());
        for path in chain.iter().rev() {
            let file_content = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| MirrorError::SpecNotFound(format!("{}: {e}", path.display())))?;
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&file_content)
                .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error in {}: {e}", path.display())]))?;
            shallow_merge(&mut base, value);
        }
        // Finally merge the child (spec_path itself) on top
        let child: serde_yaml_ng::Value = serde_yaml_ng::from_str(&content)
            .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error: {e}")]))?;
        shallow_merge(&mut base, child);
        // Strip the extends key from the merged result
        if let serde_yaml_ng::Value::Mapping(ref mut map) = base {
            map.remove("extends");
        }
        base
    };

    let spec: MirrorSpec = serde_yaml_ng::from_value(merged)
        .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error: {e}")]))?;

    // Policy check (exit 64 / SpecUsageError) must run before structural validate
    // (exit 65 / SpecInvalid) so the correct exit code is returned for URL-literal
    // webhook secrets.
    if let Some(notify) = &spec.notify {
        policy_check_notify(notify)?;
    }

    let errors = spec.validate(spec_path);
    if !errors.is_empty() {
        return Err(MirrorError::SpecInvalid(errors));
    }

    // Advisory (not fatal): cascade publishing without a build timestamp lets a
    // re-publish orphan the prior digest, which registry GC can reap and break
    // `@sha256:` pins. See `cascade_without_build_stamp` and the build_timestamp
    // reference for GC-safe options (issue #12).
    if spec.cascade_without_build_stamp() {
        log::warn!(
            "[{}] build_timestamp: none with cascade publishing — re-pointing a cascade tag orphans the prior digest, which registry GC can reap and break @sha256: pins; set build_timestamp: date|datetime or configure registry retention (see the mirror.yml build_timestamp reference)",
            spec.name
        );
    }

    Ok(spec)
}

/// Walk the `extends` chain collecting file paths: [parent, grandparent, ...].
/// Detects circular dependencies via `HashSet<PathBuf>`.
///
/// Paths are as joined, not canonicalized — a chain entry may still contain
/// `..`. Callers that compare them against another path (the CI renderer places
/// them under the repository root) canonicalize first.
pub async fn resolve_extends_chain(spec_path: &Path, content: &str) -> Result<Vec<std::path::PathBuf>, MirrorError> {
    let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(content)
        .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error: {e}")]))?;

    let mapping = match &value {
        serde_yaml_ng::Value::Mapping(m) => m,
        _ => return Ok(vec![]),
    };

    let extends_value = match mapping.get("extends") {
        Some(v) => v,
        None => return Ok(vec![]),
    };

    let spec_dir = spec_path.parent().unwrap_or(Path::new("."));
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(spec_path.canonicalize().unwrap_or_else(|_| spec_path.to_path_buf()));

    // Start with the first extends reference
    let mut current_extends = extends_value.clone();
    let mut current_dir = spec_dir.to_path_buf();

    loop {
        let base_rel = match current_extends.as_str() {
            Some(s) => s.to_string(),
            None => {
                return Err(MirrorError::SpecInvalid(vec![
                    "extends: value must be a string path".to_string(),
                ]));
            }
        };

        let base_path = current_dir.join(&base_rel);
        if !base_path.exists() {
            return Err(MirrorError::SpecInvalid(vec![format!(
                "extends: base file not found: {}",
                base_path.display()
            )]));
        }

        let canonical = base_path.canonicalize().unwrap_or_else(|_| base_path.clone());
        if !seen.insert(canonical) {
            // Build a nice cycle description
            let cycle: Vec<String> = std::iter::once(spec_path.display().to_string())
                .chain(chain.iter().map(|p: &std::path::PathBuf| p.display().to_string()))
                .chain(std::iter::once(base_path.display().to_string()))
                .collect();
            return Err(MirrorError::SpecInvalid(vec![format!(
                "extends: circular dependency: {}",
                cycle.join(" -> ")
            )]));
        }

        chain.push(base_path.clone());

        // Check if the base file also has an extends
        let base_content = tokio::fs::read_to_string(&base_path)
            .await
            .map_err(|e| MirrorError::SpecNotFound(format!("{}: {e}", base_path.display())))?;
        let base_value: serde_yaml_ng::Value = serde_yaml_ng::from_str(&base_content)
            .map_err(|e| MirrorError::SpecInvalid(vec![format!("YAML parse error in {}: {e}", base_path.display())]))?;

        match base_value.as_mapping().and_then(|m| m.get("extends")) {
            Some(next) => {
                current_extends = next.clone();
                current_dir = base_path.parent().unwrap_or(Path::new(".")).to_path_buf();
            }
            None => break,
        }
    }

    Ok(chain)
}

/// Shallow-merge: for each top-level key in `overlay`, replace the corresponding
/// key in `base` entirely. No recursion into nested maps.
pub fn shallow_merge(base: &mut serde_yaml_ng::Value, overlay: serde_yaml_ng::Value) {
    let base_map = match base {
        serde_yaml_ng::Value::Mapping(m) => m,
        _ => return,
    };
    let overlay_map = match overlay {
        serde_yaml_ng::Value::Mapping(m) => m,
        _ => return,
    };
    for (key, value) in overlay_map {
        base_map.insert(key, value);
    }
}
