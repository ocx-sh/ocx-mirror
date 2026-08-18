// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Plan entries for the env sources — `pylock` and `pypi`.
//!
//! Both resolve wheels rather than release assets, so the entry carries a
//! wheels key per platform instead of an asset URL. `pypi` additionally
//! derives one PEP 751 lock per candidate version, which is the one thing
//! `pipeline plan` does that is not side-effect-free.

use super::*;

/// The `PlanReport` wrapper for an env source's version entries.
///
/// `has_drift` is always `false`: env metadata is composed from the lock, so
/// there is no spec-declared document to diff a published tile against (see
/// the dispatch in [`build_plan_report`]).
pub fn env_plan_report(spec: &MirrorSpec, versions: Vec<PlanVersionEntry>) -> PlanReport {
    PlanReport {
        schema_version: 3,
        has_new: !versions.is_empty(),
        has_drift: false,
        versions,
        target: format!("{}/{}", spec.target.registry, spec.target.repository),
        ocx_mirror_rev: spec.ocx_mirror.as_ref().and_then(|c| c.rev.clone()),
    }
}

/// Builds the `PlanVersionEntry` list for a `pylock`-sourced spec.
///
/// Thin wrapper: resolves the app version from the source adapter's
/// already-listed `VersionInfo`, loads the committed lock, and delegates the
/// actual per-platform wheel selection to the lock-agnostic
/// [`build_env_plan_entries`].
pub async fn build_pylock_plan_entries(
    spec: &MirrorSpec,
    spec_dir: &std::path::Path,
    path: &str,
    upstream_versions: &[source::VersionInfo],
    all_tags: &[String],
    version_map: &VersionPlatformMap,
    build_ts: &Option<String>,
) -> Result<Vec<PlanVersionEntry>, MirrorError> {
    let app_version = upstream_versions
        .first()
        .map(|info| info.version.clone())
        .ok_or_else(|| MirrorError::PylockError("pylock source produced no version".to_string()))?;

    // The source adapter (list_upstream_versions, above) already parsed the
    // lock once to extract the app version; parsing it again here is the
    // price of keeping `source::VersionInfo` source-agnostic (no `Pylock`
    // leaking into it) — a committed local pylock.toml is small, so the extra
    // parse is cheaper than threading the parsed value across the source
    // boundary.
    let lock = source::pylock::load(spec_dir, path)
        .await
        .map_err(|e| source::pylock::classify_error("failed to load pylock source", e))?;

    build_env_plan_entries(spec, &lock, &app_version, all_tags, version_map, build_ts)
}

/// Lock-agnostic core of [`build_pylock_plan_entries`].
///
/// Bypasses `resolve_assets`/`filter::filter_versions` entirely (D1): for
/// each declared `wheels:` platform key whose BASE os/arch
/// `spec.platform_applies` accepts and whose FULL key (os_features included)
/// is not already published (per `version_map`), resolves a `PythonTarget`
/// from the key + its effective filter and calls `ocx_python::select_wheels`
/// directly, emitting one `PlanAssetEntry` per selected wheel carrying the
/// full key. `platforms` dedupes onto base strings so the CI matrix gate
/// keeps matching `matrix.platform`. Takes an already-parsed
/// `lock`/`app_version` so it never touches the filesystem — network-free and
/// directly unit-testable.
/// `build_ts` is the run's build stamp, resolved once by the caller for the
/// whole plan exactly as the archive branch resolves it: the emitted `version`
/// is the STAMPED publish tag, `source_version` stays the bare release.
/// Everything keyed on release identity — platform applicability and the
/// already-published dedup below — keeps using the bare version, because a
/// published version's registry identity is its bare `X.Y.Z` cascade tag (the
/// same relation `filter::filter_versions` relies on for the archive path).
///
/// That identity is only ever there because the env push writes it
/// unconditionally: `push.rs`'s env loop gates `--cascade` on
/// `Version::parse(version).is_some()` and on nothing in this run having
/// failed — it does **not** consult `spec.cascade.enabled`. Were that gate
/// ever to honor `cascade: false`, a stamped env mirror would publish
/// `X.Y.Z_<stamp>` tags only, neither the `version_map` lookup nor the
/// `all_tags` check below would recognise the release as published, and every
/// run would republish every version. Dedup would then have to key on the
/// stamped tags' release cores instead of on the bare version.
pub fn build_env_plan_entries(
    spec: &MirrorSpec,
    lock: &Pylock,
    app_version: &str,
    all_tags: &[String],
    version_map: &VersionPlatformMap,
    build_ts: &Option<String>,
) -> Result<Vec<PlanVersionEntry>, MirrorError> {
    let python = spec
        .python
        .as_ref()
        .ok_or_else(|| MirrorError::SpecInvalid(vec!["python config is required for env sources".to_string()]))?;
    let interpreter = pylock_interpreter_pin(python)?;
    let wheels_map = spec
        .wheels
        .as_ref()
        .ok_or_else(|| MirrorError::SpecInvalid(vec!["wheels config is required for env sources".to_string()]))?;

    let declared_platform_count = spec.platforms.as_ref().map_or(0, |platforms| platforms.len());

    // The pylock app version is a PEP 440 string, which may carry more
    // numeric components than `ocx_lib::Version` (a ≤3-component
    // tool-release-tag semver parser) accepts — pycowsay's `0.0.0.2`, or a
    // calendar version like `2024.1.1.1`. A tag that does not parse simply
    // cannot be present in the `Version`-keyed `version_map`, so it is
    // treated as outstanding work rather than panicking.
    //
    // ponytail: per-platform dedup of such non-semver versions is therefore
    // a no-op — a re-run re-publishes the (identical, content-addressed)
    // env, which the registry dedups. Precise PEP 440 dedup would need a
    // PEP 440-aware `version_map`; deferred (not blocking — publishes are
    // idempotent).
    let check_version = Version::parse(app_version);

    let mut missing_platforms: Vec<String> = Vec::new();
    let mut assets = Vec::new();

    for platform in wheels_map.sorted_platforms() {
        let key = platform.to_string();
        let base = spec::base_platform_key(platform);
        if !spec.platform_applies(app_version, &base) {
            continue;
        }
        if check_version
            .as_ref()
            .is_some_and(|version| version_map.has(version, platform))
        {
            continue; // already published for this full key (os_features included)
        }

        let target = PythonTarget {
            platform: pylock_target_platform(platform, &key)?,
            variant: wheel_target_constraints(wheels_map, platform),
            interpreter: interpreter.clone(),
        };

        let wheels = ocx_python::select_wheels(lock, &target)
            .map_err(|e| MirrorError::PylockError(format!("wheel selection failed for platform '{key}': {e}")))?;

        if !missing_platforms.contains(&base) {
            missing_platforms.push(base.clone());
        }
        for wheel in wheels {
            let url_str = wheel.url.ok_or_else(|| {
                MirrorError::PylockError(format!(
                    "wheel '{}' for package '{}' selected with no download URL",
                    wheel.filename, wheel.name
                ))
            })?;
            let url = url::Url::parse(&url_str)
                .map_err(|e| MirrorError::PylockError(format!("invalid wheel URL '{url_str}': {e}")))?;
            assets.push(PlanAssetEntry {
                platform: key.clone(),
                asset_name: wheel.filename,
                url,
            });
        }
    }

    if missing_platforms.is_empty() {
        return Ok(Vec::new());
    }

    // Same New/BackfillPartial convention as build_version_entries: the bare
    // (un-timestamped) tag already on the registry means some platform was
    // published before, so a shorter missing-set than the declared count is a
    // backfill, not a first publish.
    let version_on_registry = Version::parse(app_version)
        .is_some_and(|v| all_tags.iter().any(|t| Version::parse(t).is_some_and(|tv| tv == v)));
    let kind = if version_on_registry && declared_platform_count > missing_platforms.len() {
        PlanVersionKind::BackfillPartial
    } else {
        PlanVersionKind::New
    };

    Ok(vec![PlanVersionEntry {
        version: normalizer::env_version_tag(app_version, build_ts),
        platforms: missing_platforms,
        kind,
        source_version: app_version.to_string(),
        variant: None,
        assets,
        pylock: None,
    }])
}

/// Cheap pre-filter for `source.type: pypi` lock-derivation candidates:
/// `versions:` bounds, `skip_prereleases`, an already-published dedup check
/// (at least one declared `wheels:` key still outstanding), and
/// `new_per_run`/`backfill` — all applied BEFORE any `uv`/`ocx` subprocess
/// spawns, so [`build_pypi_plan_entries`] only pays the derivation cost
/// (interpreter materialization + `uv pip compile`) for versions that
/// actually have outstanding work.
///
/// Deliberately does not reuse `filter::filter_versions`: its already-
/// published dedup step `.expect()`s every tag to parse as `ocx_lib::Version`,
/// which panics on a real PyPI version string that has more components than
/// that ≤3-component parser accepts (e.g. `0.0.0.2`) or a PEP 440 `uv`-only
/// suffix (`2.0.0.dev0`) — the same reason `build_env_plan_entries` bypasses
/// it for `pylock` (D1, `plan_python_mirror_v2`). It does share that
/// function's bounds comparator (`filter::within_bounds`) and its fail-open
/// convention: a tag no version parser understands is kept as outstanding
/// work.
pub fn select_pypi_candidates<'a>(
    spec: &MirrorSpec,
    upstream_versions: &'a [source::VersionInfo],
    version_map: &VersionPlatformMap,
) -> Vec<&'a source::VersionInfo> {
    let wheels_keys: Vec<&Platform> = spec
        .wheels
        .as_ref()
        .map_or_else(Vec::new, WheelPatterns::sorted_platforms);

    let versions_config = spec.versions.as_ref();
    let min = versions_config.and_then(|c| c.min.as_deref());
    let max = versions_config.and_then(|c| c.max.as_deref());

    let mut candidates: Vec<&source::VersionInfo> = upstream_versions
        .iter()
        .filter(|info| !(spec.skip_prereleases && info.is_prerelease))
        .filter(|info| filter::within_bounds(&info.version, min, max))
        .filter(|info| {
            let tag_version = Version::parse(&info.version);
            wheels_keys.iter().any(|&platform| {
                spec.platform_applies(&info.version, &spec::base_platform_key(platform))
                    && match &tag_version {
                        Some(v) => !version_map.has(v, platform),
                        // Unparseable tag: cannot be in the Version-keyed
                        // map, so treat as outstanding.
                        None => true,
                    }
            })
        })
        .collect();

    // Total order (see `push::pep440_sort_key`): the pairwise
    // parse-both-or-compare-text comparator this replaces is intransitive, and
    // the resulting order decides which candidates `new_per_run` truncates.
    candidates.sort_by_key(|info| pep440_sort_key(&info.version));

    if let Some(config) = versions_config
        && let Some(cap) = config.new_per_run
    {
        match config.backfill {
            BackfillOrder::OldestFirst => candidates.truncate(cap),
            BackfillOrder::NewestFirst => {
                let start = candidates.len().saturating_sub(cap);
                candidates = candidates.split_off(start);
            }
        }
    }

    candidates
}

/// Maps a [`lock_derive`] `String` error to the mirror's error taxonomy
/// (plan_python_mirror_v2 W3 acceptance contract: uv-fail→65, uv-missing→1).
///
/// Data errors — this version cannot produce a trustworthy lock — map to
/// [`MirrorError::PylockError`] (exit 65, same class as `select_wheels`
/// failures): `uv`'s nonzero exit (unsolvable requirements, bad package
/// metadata; the message carries uv's stderr tail) and `derive_pylock`'s
/// fail-closed re-parse rejection. Everything else — `uv` binary
/// missing/spawn failure, timeout, interpreter materialization, lock-file
/// I/O — is a subprocess execution failure ([`MirrorError::ExecutionFailed`],
/// exit 1), the same convention `describe.rs::invoke_describe` uses for
/// `ocx package describe` subprocess failures.
///
/// ponytail: string-sniffs the two data-error markers rather than a
/// structured `lock_derive::Error` enum — promote to a real error type if
/// another call site needs to distinguish more sub-failures.
pub fn classify_lock_derive_error(err: String) -> MirrorError {
    if err.contains("failed to re-parse") || err.contains("uv pip compile exited") {
        MirrorError::PylockError(err)
    } else {
        MirrorError::ExecutionFailed(vec![err])
    }
}

/// The on-disk filename for a derived PEP 751 lock. `uv pip compile` enforces
/// PEP 751 on `-o`: the name must be `pylock.toml` or `pylock.<name>.toml`
/// where `<name>` is non-empty and **contains no dots**. Both the version
/// (`0.0.0.1`) and a dotted distribution name (`zope.interface`) would land
/// dots in `<name>`, so each dot becomes a dash — found by the live W4 pypi
/// pilot, where `pylock.pycowsay-0.0.0.1.toml` failed uv with exit 2.
///
/// The layout stays flat (one directory, one file per version) because nothing
/// parses this name: the plan carries each derived lock's path verbatim in its
/// entry's `pylock` field, `prepare --plan` reads that path, and `describe`
/// picks any lock in the directory by extension. Dashing the dots is therefore
/// lossy but harmless — no caller recovers a version from the filename.
///
/// Shared by the plan-phase candidate loop and `prepare.rs`'s standalone
/// re-derivation so the two sites cannot drift.
pub fn derived_lock_filename(package: &str, version: &str) -> String {
    let name = format!("{package}-{version}").replace('.', "-");
    format!("pylock.{name}.toml")
}

/// `python.lock`'s defaults, applied when a `pypi` spec omits the `lock:`
/// block entirely (zero-config: universal lock, no excludes, 300s timeout).
pub fn default_lock_options() -> LockOptions {
    LockOptions {
        universal: true,
        extras: Vec::new(),
        exclude: Vec::new(),
        timeout_seconds: 300,
    }
}

/// Resolves the [`lock_derive::UvPython`] selector for this spec's lock
/// derivations — ONCE per plan/prepare run, shared by every candidate.
///
/// Universal locks (the default) resolve via `--python-version X.Y` (from
/// `python.version`) with no interpreter materialization at all — cheaper
/// (no `ocx package pull` in the plan phase) and, critically, compatible
/// with fully-static interpreter builds that defeat uv's libc inspection
/// (live W4 pilot: "Could not detect a glibc or a musl libc"). Only
/// `universal: false` materializes the pinned `interpreter_package` for an
/// exact-interpreter resolution.
pub async fn resolve_uv_python(python: &PythonConfig) -> Result<lock_derive::UvPython, MirrorError> {
    let universal = python.lock.as_ref().is_none_or(|lock| lock.universal);
    if universal {
        Ok(lock_derive::UvPython::Version(
            pylock_interpreter_pin(python)?.python_version,
        ))
    } else {
        let interpreter_path = lock_derive::materialize_interpreter(&python.interpreter_package)
            .await
            .map_err(|e| MirrorError::ExecutionFailed(vec![e]))?;
        Ok(lock_derive::UvPython::Interpreter(interpreter_path))
    }
}

/// Derives a single PEP 751 lock for one already-resolved Python selector and
/// one already-known `app_version`. Shared plumbing between the plan-phase
/// candidate loop ([`build_pypi_plan_entries`]) and `prepare.rs`'s standalone
/// (no `--plan`) re-derivation path, both of which otherwise repeat the same
/// `python.lock` defaulting + provenance-timestamp + request assembly.
pub async fn derive_one_pypi_lock(
    spec: &MirrorSpec,
    uv_python: &lock_derive::UvPython,
    app_version: &str,
    output_path: &Path,
) -> Result<Pylock, MirrorError> {
    let Source::Pypi { .. } = &spec.source else {
        return Err(MirrorError::SpecInvalid(vec![
            "lock derivation is only defined for source.type 'pypi'".to_string(),
        ]));
    };
    let python = spec.python.as_ref().ok_or_else(|| {
        MirrorError::SpecInvalid(vec!["python config is required for source.type 'pypi'".to_string()])
    })?;
    let package = spec.source.pylock_app_name(&spec.name);
    let indexes = spec.source.pypi_indexes();
    let lock_options = python.lock.clone().unwrap_or_else(default_lock_options);
    let generated_at = Utc::now().to_rfc3339();

    let request = lock_derive::DeriveLockRequest {
        python: uv_python,
        package,
        version: app_version,
        indexes: &indexes,
        options: &lock_options,
        output_path,
        generated_at: &generated_at,
    };
    lock_derive::derive_pylock(&request)
        .await
        .map_err(classify_lock_derive_error)
}

/// Builds the `PlanVersionEntry` list for a `pypi`-sourced spec (design
/// decision A, `plan_python_mirror_v2`).
///
/// [`select_pypi_candidates`] picks the versions worth deriving a lock for
/// (cheap, no subprocess spawns); the Python selector is then resolved ONCE
/// for the whole plan run via [`resolve_uv_python`] (every candidate
/// resolves against the same version/interpreter and index), and each
/// candidate's lock is derived in turn and written under `locks_dir`. The
/// lock-agnostic `build_env_plan_entries` (shared with the `pylock` branch
/// above) does the actual per-(variant, platform) wheel selection once a
/// lock is in hand.
pub async fn build_pypi_plan_entries(
    spec: &MirrorSpec,
    upstream_versions: &[source::VersionInfo],
    all_tags: &[String],
    version_map: &VersionPlatformMap,
    locks_dir: &Path,
    build_ts: &Option<String>,
) -> Result<Vec<PlanVersionEntry>, MirrorError> {
    let python = spec.python.as_ref().ok_or_else(|| {
        MirrorError::SpecInvalid(vec!["python config is required for source.type 'pypi'".to_string()])
    })?;

    let candidates = select_pypi_candidates(spec, upstream_versions, version_map);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    tokio::fs::create_dir_all(locks_dir).await.map_err(|e| {
        MirrorError::ExecutionFailed(vec![format!(
            "failed to create locks dir '{}': {e}",
            locks_dir.display()
        )])
    })?;

    let uv_python = resolve_uv_python(python).await?;

    let package = spec.source.pylock_app_name(&spec.name);

    let mut entries = Vec::new();
    for version_info in candidates {
        // Locks are keyed by SOURCE version: a lock belongs to the upstream
        // release, while the build stamp is a property of this publish.
        let output_path = locks_dir.join(derived_lock_filename(package, &version_info.version));
        let lock = derive_one_pypi_lock(spec, &uv_python, &version_info.version, &output_path).await?;

        let mut version_entries =
            build_env_plan_entries(spec, &lock, &version_info.version, all_tags, version_map, build_ts)?;
        let pylock_path = output_path.to_string_lossy().into_owned();
        for entry in &mut version_entries {
            entry.pylock = Some(pylock_path.clone());
        }
        entries.extend(version_entries);
    }

    Ok(entries)
}

/// Derives the `ocx_python` selection constraints for one `wheels:` platform
/// key: the key's declared libc (or the filter-implied one for plain linux
/// keys — musl iff the effective filter carries `musllinux*` prefixes and no
/// `manylinux*` ones, else gnu) plus the effective filter as the
/// admissibility/ranking list. Floors stay `None` — `select` applies its
/// defaults (`manylinux_2_28`/`musllinux_1_2`); `python.abi` remains the one
/// ABI pin (no per-key override).
pub fn wheel_target_constraints(wheels: &WheelPatterns, platform: &Platform) -> VariantConstraints {
    let filter = wheels.effective_filter(platform);
    let libc = match spec::libc_feature(platform) {
        Some("libc.musl") => LibcFamily::Musl,
        Some("libc.glibc") => LibcFamily::Gnu,
        _ => {
            let has_musllinux = filter.iter().any(|entry| entry.starts_with("musllinux"));
            let has_manylinux = filter.iter().any(|entry| entry.starts_with("manylinux"));
            if has_musllinux && !has_manylinux {
                LibcFamily::Musl
            } else {
                LibcFamily::Gnu
            }
        }
    };
    VariantConstraints {
        libc: Some(libc),
        min_manylinux: None,
        min_musllinux: None,
        abi: None,
        wheel_priority: Some(filter),
    }
}

/// Builds the interpreter pin from the spec's `python:` block.
pub fn pylock_interpreter_pin(python: &PythonConfig) -> Result<InterpreterPin, MirrorError> {
    let version = Version::parse(&python.version)
        .ok_or_else(|| MirrorError::PylockError(format!("invalid python.version '{}'", python.version)))?;
    let minor = version
        .minor()
        .ok_or_else(|| MirrorError::PylockError(format!("python.version '{}' needs major.minor", python.version)))?;
    Ok(InterpreterPin {
        python_version: format!("{}.{minor}", version.major()),
        python_full_version: python.version.clone(),
        abi: python.abi.clone(),
        implementation: Implementation::CPython,
    })
}

/// Maps a wheels key's parsed `ocx_lib::oci::Platform` to `ocx_python`'s
/// `TargetPlatform` (os/arch only — the key's `+libc.*` os_features travel
/// through [`wheel_target_constraints`], not this mapping).
pub fn pylock_target_platform(platform: &Platform, key: &str) -> Result<TargetPlatform, MirrorError> {
    let Platform::Specific { os, arch, .. } = platform else {
        return Err(MirrorError::PylockError(format!(
            "platform key '{key}' must be a concrete os/arch pair for pylock sources"
        )));
    };
    let operating_system = match os {
        OperatingSystem::Linux => TargetOperatingSystem::Linux,
        OperatingSystem::Darwin => TargetOperatingSystem::Darwin,
        OperatingSystem::Windows => TargetOperatingSystem::Windows,
    };
    let architecture = match arch {
        Architecture::Amd64 => TargetArchitecture::Amd64,
        Architecture::Arm64 => TargetArchitecture::Arm64,
    };
    Ok(TargetPlatform {
        operating_system,
        architecture,
    })
}
