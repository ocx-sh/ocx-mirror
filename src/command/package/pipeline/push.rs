// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline push` — aggregate JUNIT results, apply go/no-go logic,
//! call `ocx package push --cascade --format json` for passing `(V, P)` pairs,
//! and emit `run-summary.json`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ocx_lib::cli::{DataInterface, ExitCode};
use ocx_lib::log;
use ocx_lib::oci::ClientBuilder;
use ocx_lib::package::version::Version;
use ocx_lib::publisher::Publisher;

use crate::command::package::pipeline::patch::patch_push_args;
use crate::command::package::pipeline::plan;
use crate::command::package::target_registry;
use crate::error::MirrorError;
use crate::junit::{self, JunitTestcase};
use crate::pipeline::python_prepare::EnvManifest;
use crate::pipeline::python_push;
use crate::run_summary::{
    AnnounceOutcome, ExcludedPlatform, LayerReuse, PlatformFailure, RunSummary, TestFailure, VersionStatus,
    VersionSummary,
};
use crate::spec::{self, AnnounceConfig, MirrorSpec, PlatformConfig, Severity};

/// The `ocx` subprocess helpers live at the pipeline layer so `python_push`
/// shares one implementation with the archive legs; re-exported here because
/// `patch.rs` and `announce.rs` resolve them through this module's path.
pub(crate) use crate::pipeline::ocx_cli::{forward_ocx_env, resolve_ocx_binary};

/// `ocx-mirror package pipeline push` subcommand.
///
/// Single serial push driver. Sole writer of cascade tags in the pipeline.
///
/// Exit 0 even when some versions fail — the summary records per-version
/// outcomes. Exit 69 on registry unreachability mid-push. Exit 74 on I/O
/// failure reading JUNIT/bundles or writing the summary.
#[derive(clap::Parser)]
pub struct Push {
    /// Path to the mirror spec file.
    #[arg(long, default_value = "./mirror.yml")]
    pub spec: PathBuf,

    /// Directory containing `bundle-{V}-{platform_slug}.tar.xz` files
    /// (downloaded GHA artifacts).
    #[arg(long, required = true)]
    pub bundles_dir: PathBuf,

    /// Directory containing `junit-{V}-{platform_slug}-{container_id}.xml` files
    /// (test results from the `test` matrix).
    #[arg(long, required = true)]
    pub junit_dir: PathBuf,

    /// Path to write the `run-summary.json` output file.
    #[arg(long, required = true)]
    pub write_summary: PathBuf,
}

/// Per-`(V, P)` go/no-go decision after evaluating JUNIT files.
#[derive(Debug)]
enum VpDecision {
    /// All containers green for all declared tests.
    Green,
    /// At least one container failed or had a missing JUNIT.
    Red {
        platform_failure: PlatformFailure,
        test_failures: Vec<TestFailure>,
    },
}

/// Parsed JSON output from `ocx package push --cascade --format json`.
///
/// Fields align with the `PushReport` shape from subsystem-cli.md §2.4.
///
/// Every field defaults, so `{}` satisfies the parse (`patch::republish`
/// relies on that) and an `ocx` predating the layer-mount counters simply
/// reports zeros rather than failing.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct PushReport {
    /// SHA-256 manifest digest of the pushed image. Captured for audit trails
    /// but not surfaced in run-summary.json in this version.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) manifest_digest: Option<String>,
    #[serde(default)]
    pub(crate) cascade_tags_written: Vec<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    /// Layer-push outcome counts (mounted/uploaded/verified) — the shared-wheel
    /// reuse the env path records into `run-summary.json`.
    #[serde(default)]
    pub(crate) layers: LayerReuse,
}

impl Push {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        // ── Load spec ────────────────────────────────────────────────────────
        let spec = spec::load_spec(&self.spec).await?;

        // Env sources (pylock/pypi) take a parallel env-push path: env
        // packages (wheel layers + composed metadata) instead of the
        // archive/binary bundle. Mirrors `prepare.rs`'s `is_env()` dispatch —
        // prepare writes env-manifest.json (never bundle-*.tar.xz) for both,
        // so the archive loop below would silently find nothing.
        if spec.source.is_env() {
            return self.execute_pylock_push(&spec).await;
        }

        // GHA workflow stamps the push job's html_url here so the Discord
        // embed can link push-tier successes + failures back to push logs.
        // Test-tier failures keep linking to their matrix-leg URL parsed out
        // of the JUnit `ci.job.url` property.
        let push_job_url = std::env::var("OCX_MIRROR_JOB_URL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());

        // ── Enumerate declared (version, platform) pairs from the bundles dir ──
        // Bundle files are named: `bundle-{V}-{platform_slug}.tar.xz`
        let bundle_map = enumerate_bundles(&self.bundles_dir).await?;

        if bundle_map.is_empty() {
            log::info!("[{}] No bundles found in {}", spec.name, self.bundles_dir.display());
        }

        // ── Sort versions oldest-first (semver) ──────────────────────────────
        let mut versions: Vec<String> = bundle_map.keys().cloned().collect();
        versions.sort_by(|a, b| {
            let va = Version::parse(a);
            let vb = Version::parse(b);
            match (va, vb) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => a.cmp(b),
            }
        });

        // ── Determine platform declaration order from the spec ───────────────
        // Platform order in run-summary follows spec declaration order.
        let platform_order = spec_platform_order(&spec);

        // ── The newest version in this run (for latest-tag logic) ────────────
        // "Newest" is the last element of the semver-sorted list.
        let newest_version = versions.last().cloned();

        // Read-only registry access for the backfill cascade repair below, and
        // the one annotation set that repair republishes under — the same map
        // `invoke_push` builds per leg.
        let client = ClientBuilder::from_env().map_err(|e| MirrorError::ExecutionFailed(vec![e.to_string()]))?;
        let publisher = Publisher::new(client);
        let annotations = crate::annotations::build_annotations(&spec.annotations);

        // ── Process each version in semver order ─────────────────────────────
        let mut version_summaries: Vec<VersionSummary> = Vec::new();

        for version in &versions {
            let platforms_for_version = bundle_map.get(version).cloned().unwrap_or_default();

            // Bundles are keyed by platform slug (`linux_amd64`); spec keys + the
            // `--platform` CLI arg both use slash form (`linux/amd64`). Normalise
            // to slash form here so the rest of the loop matches spec lookups
            // and downstream push invocations.
            let mut sorted_platforms: Vec<String> = platforms_for_version
                .into_iter()
                .map(|slug| slug_to_platform(&spec, &slug))
                .collect();
            sorted_platforms.sort_by_key(|p| platform_order.iter().position(|s| s == p).unwrap_or(usize::MAX));

            // ── Phase 1: decide every (V, P) pair BEFORE pushing anything ────
            //
            // Nothing is pushed while the verdict for the version is still
            // open. `--cascade` moves `latest` / `X` / `X.Y` as a side effect
            // of the push that carries it, so interleaving decide-and-push
            // let the first green platform advertise a version whose next leg
            // then went red.
            let mut platforms_failed: Vec<PlatformFailure> = Vec::new();
            let mut all_test_failures: Vec<TestFailure> = Vec::new();
            let mut ready: Vec<(String, PathBuf)> = Vec::new();

            for platform_str in &sorted_platforms {
                // Determine expected container IDs from spec.
                let container_ids = container_ids_for_platform(&spec, platform_str);

                // Evaluate JUNIT for this (V, P) across all declared containers.
                let decision = evaluate_junit(
                    &self.junit_dir,
                    version,
                    platform_str,
                    &container_ids,
                    &test_names_for_platform(&spec, platform_str),
                )
                .await;

                match decision {
                    VpDecision::Red {
                        platform_failure,
                        test_failures,
                    } => {
                        platforms_failed.push(platform_failure);
                        all_test_failures.extend(test_failures);
                    }
                    VpDecision::Green => {
                        let bundle_path = bundle_path_for(&self.bundles_dir, version, &platform_to_slug(platform_str));
                        if bundle_path.exists() {
                            ready.push((platform_str.clone(), bundle_path));
                        } else {
                            // Bundle absent — treat as failure.
                            platforms_failed.push(PlatformFailure {
                                platform: platform_str.clone(),
                                reason: "missing_bundle".to_string(),
                                failed_tests: vec![],
                                job_url: push_job_url.clone(),
                            });
                        }
                    }
                }
            }

            // ── Phase 2: push, cascading only on a whole version ─────────────
            let mut platforms_pushed: Vec<String> = Vec::new();
            let mut cascade_tags: Vec<String> = Vec::new();
            let mut all_skipped_existing = platforms_failed.is_empty();
            let target_ref = format!("{}:{}", spec.target.reference(), version);

            for (platform_str, bundle_path) in &ready {
                // EVERY push of a whole version cascades. A cascade push merges
                // only its OWN platform into each rolling tag — `client.rs`
                // `merge_platform_into_index` retains every other platform's
                // existing entry — so giving `--cascade` to one push per version
                // leaves the other platforms stranded on the exact version tag,
                // and each alias keeps whatever the PREVIOUS version left there.
                // The condition is only about failure: nothing about this
                // version may have gone wrong, neither a test leg in phase 1 nor
                // an earlier push in this loop.
                let cascade = platforms_failed.is_empty();

                match invoke_push(&spec, platform_str, &target_ref, bundle_path, cascade).await {
                    Ok(report) => {
                        let status_str = report.status.as_deref().unwrap_or("pushed");
                        if status_str == "skipped_existing" {
                            // Don't flip all_skipped_existing to false
                        } else {
                            all_skipped_existing = false;
                            platforms_pushed.push(platform_str.clone());
                            cascade_tags.extend(report.cascade_tags_written);
                        }
                    }
                    Err(msg) => {
                        all_skipped_existing = false;
                        log::warn!("[{}] Push failed for {}/{}: {}", spec.name, version, platform_str, msg);
                        platforms_failed.push(PlatformFailure {
                            platform: platform_str.clone(),
                            reason: "push_error".to_string(),
                            failed_tests: vec![],
                            job_url: push_job_url.clone(),
                        });
                    }
                }
            }

            // The platforms an earlier run published carry no cascade of their
            // own — see [`entries_awaiting_cascade`] — so the rolling tags are
            // completed from the version's merged index once its last platform
            // lands.
            if platforms_failed.is_empty() && !platforms_pushed.is_empty() {
                cascade_tags.extend(
                    cascade_backfilled_entries(&publisher, &spec, version, &platforms_pushed, &annotations).await,
                );
            }

            // The version-specific tag is always written when at least one
            // platform pushed, but `ocx package push --cascade` only reports
            // the *additional* cascade tags (X.Y, X, latest) — re-injecting
            // the explicit version keeps the embed truthful.
            if !platforms_pushed.is_empty() && !cascade_tags.iter().any(|t| t == version) {
                cascade_tags.insert(0, version.clone());
            }
            // Order-preserving full dedup: every platform of a whole version
            // cascades, so every one of their reports re-lists the same
            // hierarchy, and `Vec::dedup` only collapses *consecutive*
            // duplicates.
            //
            // The accumulation is therefore the UNION over the version's
            // platforms — "at least one platform wrote this tag", not "this tag
            // carries every platform". The two differ only when
            // `resolve_cascade_tags`, which IS platform-aware, stops one
            // platform's chain early on a blocker the others do not have. The
            // union is the right set to announce even then: the tag exists in
            // the registry, and `ocx package announce` re-fetches each one and
            // records the platforms it actually holds. Reporting the
            // intersection instead would leave a live registry tag unannounced,
            // which is the exact drift the announce exists to prevent.
            {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                cascade_tags.retain(|t| seen.insert(t.clone()));
            }

            // ── Determine version status per D12 ─────────────────────────────
            let is_newest = Some(version.as_str()) == newest_version.as_deref();
            let status = determine_status(
                &platforms_pushed,
                &platforms_failed,
                all_skipped_existing && !sorted_platforms.is_empty(),
                is_newest,
            );

            version_summaries.push(VersionSummary {
                version: version.clone(),
                status,
                platforms_pushed,
                platforms_failed,
                cascade_tags_written: cascade_tags,
                test_failures: all_test_failures,
                platforms_excluded: collect_excluded_platforms(&spec, version),
                // Archive/binary pushes have no shared-layer concept.
                layer_reuse: LayerReuse::default(),
            });
        }

        self.finalize_run(&spec, version_summaries, push_job_url).await
    }

    /// Run-level flags, `run-summary.json`, the single index announce, and the
    /// exit verdict — everything both push paths do once every version has
    /// been decided.
    ///
    /// Shared rather than duplicated because the announce is the part a second
    /// copy would silently drift on: an env mirror that published a version and
    /// did not announce it leaves the index behind the registry with nothing
    /// failing.
    async fn finalize_run(
        &self,
        spec: &MirrorSpec,
        version_summaries: Vec<VersionSummary>,
        push_job_url: Option<String>,
    ) -> Result<(), MirrorError> {
        // ── Compute run-level flags ───────────────────────────────────────────
        let any_red = version_summaries
            .iter()
            .any(|vs| matches!(vs.status, VersionStatus::Failed | VersionStatus::Partial));
        let any_new_green = version_summaries.iter().any(|vs| {
            matches!(vs.status, VersionStatus::Published | VersionStatus::Partial) && !vs.platforms_pushed.is_empty()
        });

        // ── Build and write run-summary.json ─────────────────────────────────
        let run_url = std::env::var("GITHUB_SERVER_URL")
            .ok()
            .and_then(|server| {
                let repo = std::env::var("GITHUB_REPOSITORY").ok()?;
                let run_id = std::env::var("GITHUB_RUN_ID").ok()?;
                Some(format!("{server}/{repo}/actions/runs/{run_id}"))
            })
            .unwrap_or_else(|| "https://github.com/actions/runs/unknown".to_string());

        let mut summary = RunSummary {
            schema_version: 1,
            mirror: spec.name.clone(),
            target: format!("{}/{}", spec.target.registry, spec.target.repository),
            run_url,
            push_job_url,
            source_url: compute_source_url(&spec.source),
            logo_url: compute_logo_url(),
            versions: version_summaries,
            // Pre-announce placeholder, overwritten below by the real outcome.
            // Its PRESENCE is the signal: an absent key means "this mirror has
            // no `announce:` block", so writing `None` here would have made a
            // run killed mid-announce — a reclaimed runner, a cancelled
            // backfill — read as a mirror that never opted in, with a dozen
            // images live in GHCR and the index knowing about none of them.
            announce: spec.announce.as_ref().map(|config| AnnounceOutcome::Interrupted {
                package: config.package.clone(),
            }),
            any_red,
            any_new_green,
        };

        // Durable record first, announce second. The announce below does
        // unbounded network work; a run killed during it must still leave
        // behind the record of everything that *did* publish. Written after it,
        // the file would never exist — the artifact upload would find nothing,
        // the notify gate would evaluate false, and a dozen live images would
        // go unannounced *and* unreported.
        write_run_summary(&self.write_summary, &summary).await?;

        // One announce per run, after every version has been pushed — never
        // one per version or per platform. Concurrent announces on the same
        // package are a race the index singleflight exists to survive; there
        // is no reason to generate one from inside a single run.
        let announce_token = announce_token();
        summary.announce = run_announce(
            spec.announce.as_ref(),
            &summary.versions,
            &self.write_summary.with_extension("announce-tags"),
            announce_token.as_deref(),
            &resolve_ocx_binary().unwrap_or_else(|_| PathBuf::from("ocx")),
        )
        .await;

        if summary.announce.is_some() {
            write_run_summary(&self.write_summary, &summary).await?;
        }

        log::info!(
            "[{}] Run summary written to {} (any_red={}, any_new_green={})",
            spec.name,
            self.write_summary.display(),
            summary.any_red,
            summary.any_new_green,
        );

        // Fail the push job whenever any (V, P) pair was red — even when
        // other platforms published successfully. Per-platform publication
        // happens inline in the loop above, so greens are already in the
        // registry; this exit code surfaces the partial failure to the
        // pipeline and to the maintainer. The notify step still runs because
        // the workflow gates `notify` on the push job's outputs
        // (`any_red` / `any_new_green`), not its `success()` status, and the
        // `summarise` step uses `if: always()` to write outputs even when this
        // call returns Err.
        let mut failures: Vec<String> = Vec::new();
        if summary.any_red {
            failures.push(if summary.any_new_green {
                format!(
                    "partial run across {} version(s): some platforms failed — see run-summary.json",
                    summary.versions.len(),
                )
            } else {
                format!(
                    "all platforms failed across {} version(s); no package published — see run-summary.json",
                    summary.versions.len(),
                )
            });
        }
        // A failed announce fails the job, on the same reasoning as `any_red`
        // directly above: the images ARE in the registry, and the exit code is
        // how a partial outcome reaches the pipeline and the maintainer. Left
        // green, an expired `OCX_ANNOUNCE_TOKEN` keeps every nightly passing
        // while the index drifts arbitrarily far behind the registry, and no
        // scheduled-run alert ever fires because nothing failed.
        //
        // `SkippedNoCredential` deliberately does not: a mirror without the
        // secret is a valid configuration (forks, test repos). It stays visible
        // through the summary, the `announce` job output and the Index row.
        if let Some(AnnounceOutcome::Failed { package, error }) = &summary.announce {
            failures.push(format!(
                "index announce for {package} failed: {error} — the registry is ahead of the index",
            ));
        }
        if !failures.is_empty() {
            return Err(MirrorError::ExecutionFailed(failures));
        }

        Ok(())
    }

    /// Env-push path for `source.type: pylock`/`pypi` specs — the parallel to
    /// the archive/binary path in [`execute`](Self::execute).
    ///
    /// Reads `{bundles_dir}/{version}/env-manifest.json` per version (written
    /// by `python_prepare::prepare_env_version`), evaluates JUnit go/no-go per
    /// `(version, wheels key)` with the same `evaluate_junit`
    /// AND-across-containers logic as the archive loop, and pushes each green
    /// leg via `ocx package push` with the ordered wheel layers as positional
    /// args (each carrying a `:from=<wheel_repository>` mount tail) and the
    /// composed `metadata.json` via `-m`.
    ///
    /// Shared wheel layers (Decision D): before each leg's push,
    /// `python_push::register_wheel_layers` registers any not-yet-published
    /// wheel with the target registry, so the leg's own push can
    /// cross-repository mount it instead of re-uploading. Registration
    /// failures are warn-only; a miss falls back to a full upload, so the push
    /// always succeeds either way.
    async fn execute_pylock_push(&self, spec: &MirrorSpec) -> Result<(), MirrorError> {
        let push_job_url = std::env::var("OCX_MIRROR_JOB_URL")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());

        let mut manifests = python_push::enumerate_env_manifests(&self.bundles_dir)
            .await
            .map_err(MirrorError::TemplateError)?;

        if manifests.is_empty() {
            log::info!(
                "[{}] No env manifests found in {}",
                spec.name,
                self.bundles_dir.display()
            );
        }

        manifests.sort_by_key(|manifest: &EnvManifest| pep440_sort_key(&manifest.version));

        let platform_order = spec_platform_order(spec);
        let newest_version = manifests.last().map(|manifest| manifest.version.clone());

        // One assembled annotation set for the whole run — the same map the
        // archive leg builds per push, hoisted because every env leg and every
        // wheel registration of this run publishes under it.
        let annotations = crate::annotations::build_annotations(&spec.annotations);

        let mut version_summaries: Vec<VersionSummary> = Vec::new();

        // Read-only registry access for the wheel-registration tag-exists
        // check (Decision D, shared wheel layers). `registered_wheels` dedupes
        // `wheel_repository:wheel_sha256` pairs across the whole run so a
        // wheel shared by multiple legs is checked/pushed at most once.
        let client = ClientBuilder::from_env().map_err(|e| MirrorError::ExecutionFailed(vec![e.to_string()]))?;
        let publisher = Publisher::new(client);
        let mut registered_wheels: std::collections::HashSet<String> = std::collections::HashSet::new();

        for manifest in &manifests {
            let version = &manifest.version;

            // Spec lookups are keyed by BASE os/arch — an env entry's full
            // wheels-key platform (`linux/amd64+libc.glibc`) would miss the
            // `platforms:` map silently. Same-base entries tiebreak on the
            // full key string for determinism.
            let mut sorted_envs: Vec<_> = manifest.envs.iter().collect();
            sorted_envs.sort_by_key(|env| {
                (
                    platform_order
                        .iter()
                        .position(|p| p == base_platform_str(&env.platform))
                        .unwrap_or(usize::MAX),
                    env.platform.clone(),
                )
            });

            // ── Phase 1: decide EVERY entry of the version before pushing ────
            //
            // Same hazard, and same remedy, as the archive loop in
            // [`execute`]: `--cascade` moves `latest` / `X` / `X.Y` as a side
            // effect of the push that carries it, and the `:latest` alias
            // below does it explicitly, so deciding and pushing entry by entry
            // let a green glibc leg advertise a version whose musl leg had not
            // been looked at yet. `announce_tag_union` depends on the
            // resulting invariant: a rolling alias can never reach a partial
            // version.
            let mut platforms_failed: Vec<PlatformFailure> = Vec::new();
            let mut all_test_failures: Vec<TestFailure> = Vec::new();
            let mut ready: Vec<&crate::pipeline::python_prepare::EnvEntry> = Vec::new();

            for env_entry in &sorted_envs {
                let platform_str = &env_entry.platform;
                let base_platform = base_platform_str(platform_str);
                let libc = entry_libc_feature(platform_str);
                let container_ids = gating_container_ids_for_entry(spec, base_platform, libc);

                // Fail closed: an entry whose declared libc no test leg covers
                // (e.g. a `+libc.musl` key on a platform with only glibc
                // containers) must red, not silently push untested.
                let decision = if container_ids.is_empty() {
                    VpDecision::Red {
                        platform_failure: PlatformFailure {
                            platform: platform_str.clone(),
                            reason: "no_libc_compatible_test_leg".to_string(),
                            failed_tests: vec![],
                            job_url: push_job_url.clone(),
                        },
                        test_failures: vec![TestFailure {
                            version: version.to_string(),
                            platform: platform_str.clone(),
                            container: "_missing_".to_string(),
                            test: "<junit>".to_string(),
                            message: format!(
                                "no container of platform '{base_platform}' covers libc '{}'",
                                libc.unwrap_or("<none>")
                            ),
                        }],
                    }
                } else {
                    // The JUnit files a `+libc.*` entry gates on are named by
                    // its BASE platform (CI matrix legs are per base platform);
                    // the libc discrimination is carried by `container_ids`,
                    // which `gating_container_ids_for_entry` already filtered.
                    // `evaluate_junit` takes the SLASH form and slugs it itself.
                    evaluate_junit(
                        &self.junit_dir,
                        version,
                        base_platform,
                        &container_ids,
                        &test_names_for_platform(spec, base_platform),
                    )
                    .await
                };

                match decision {
                    VpDecision::Red {
                        mut platform_failure,
                        mut test_failures,
                    } => {
                        // `evaluate_junit` names failures by the base platform;
                        // re-stamp the FULL wheels key on the platform failure
                        // AND on every test failure, so a dual-libc red is
                        // attributable to the right entry in run-summary.json
                        // instead of collapsing both entries onto `linux/amd64`.
                        platform_failure.platform = platform_str.clone();
                        for failure in &mut test_failures {
                            failure.platform = platform_str.clone();
                        }
                        platforms_failed.push(platform_failure);
                        all_test_failures.extend(test_failures);
                    }
                    VpDecision::Green => ready.push(env_entry),
                }
            }

            // ── Phase 2: push the greens, cascading only on a whole version ──
            let mut platforms_pushed: Vec<String> = Vec::new();
            let mut pushed_entries: Vec<&crate::pipeline::python_prepare::EnvEntry> = Vec::new();
            let mut cascade_tags: Vec<String> = Vec::new();
            let mut all_skipped_existing = platforms_failed.is_empty();
            let mut layer_reuse = LayerReuse::default();
            let target_ref = format!("{}:{}", spec.target.reference(), version);
            // ocx cascade derives rolling tags by parsing the version as
            // X.Y.Z; a PEP 440 version ocx cannot parse (e.g. pycowsay's
            // `0.0.0.2`) is pushed as the primary tag only, without cascade.
            let version_is_cascadable = Version::parse(version).is_some();

            for env_entry in &ready {
                let platform_str = &env_entry.platform;
                // Nothing about this version may have gone wrong — neither a
                // test leg in phase 1 nor an earlier push in this loop — or
                // the rolling aliases stay where the previous version left them.
                let cascade = version_is_cascadable && platforms_failed.is_empty();

                // Register any not-yet-published wheel layers so this leg's
                // push can `:from=` mount them instead of re-uploading
                // (Decision D). Failures are warn-only inside
                // `register_wheel_layers` — never abort here.
                python_push::register_wheel_layers(
                    &publisher,
                    &spec.target.registry,
                    platform_str,
                    &env_entry.layers,
                    &annotations,
                    &mut registered_wheels,
                )
                .await;

                let push_result = python_push::invoke_env_push(
                    spec,
                    platform_str,
                    &target_ref,
                    &env_entry.metadata_path,
                    &env_entry.layers,
                    &annotations,
                    cascade,
                )
                .await;

                match push_result {
                    Ok(report) => {
                        let status_str = report.status.as_deref().unwrap_or("pushed");
                        if status_str == "skipped_existing" {
                            // Don't flip all_skipped_existing to false
                        } else {
                            all_skipped_existing = false;
                            layer_reuse.mounted += report.layers.mounted;
                            layer_reuse.uploaded += report.layers.uploaded;
                            layer_reuse.verified += report.layers.verified;
                            platforms_pushed.push(platform_str.clone());
                            pushed_entries.push(env_entry);
                            cascade_tags.extend(report.cascade_tags_written);
                        }
                    }
                    Err(msg) => {
                        all_skipped_existing = false;
                        log::warn!(
                            "[{}] Env push failed for {}/{}: {}",
                            spec.name,
                            version,
                            platform_str,
                            msg
                        );
                        platforms_failed.push(PlatformFailure {
                            platform: platform_str.clone(),
                            reason: "push_error".to_string(),
                            failed_tests: vec![],
                            job_url: push_job_url.clone(),
                        });
                    }
                }
            }

            // ── Phase 2b: the entries an earlier run published ───────────────
            //
            // Phase 2 gave `--cascade` to every leg of this run, which covers a
            // version published in one run and not one completed across two —
            // see [`entries_awaiting_cascade`]. Only reachable for a cascadable
            // version: `plan::build_env_plan_entries` cannot dedup a version
            // `ocx_lib::Version` refuses to parse, so a non-semver version
            // re-publishes every platform on every run and never splits.
            if version_is_cascadable && platforms_failed.is_empty() && !platforms_pushed.is_empty() {
                cascade_tags.extend(
                    cascade_backfilled_entries(&publisher, spec, version, &platforms_pushed, &annotations).await,
                );
            }

            // ── Phase 3: `:latest` for a version ocx cannot cascade ──────────
            //
            // After the whole version, never inside phase 2: the alias is a
            // rolling tag by another name, so it is bound by the same rule —
            // it may only move onto a version every one of whose entries
            // landed.
            if !version_is_cascadable
                && Some(version.as_str()) == newest_version.as_deref()
                && platforms_failed.is_empty()
                && !pushed_entries.is_empty()
                && run_newest_is_registry_newest(&publisher, spec, version).await
            {
                for env_entry in &pushed_entries {
                    if alias_newest_as_latest(spec, env_entry, &env_entry.platform, version, &annotations).await
                        && !cascade_tags.iter().any(|t| t == "latest")
                    {
                        cascade_tags.push("latest".to_string());
                    }
                }
            }

            if !platforms_pushed.is_empty() && !cascade_tags.iter().any(|t| t == version) {
                cascade_tags.insert(0, version.clone());
            }
            {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                cascade_tags.retain(|t| seen.insert(t.clone()));
            }

            let is_newest = Some(version.as_str()) == newest_version.as_deref();
            let status = determine_status(
                &platforms_pushed,
                &platforms_failed,
                all_skipped_existing && !sorted_envs.is_empty(),
                is_newest,
            );

            version_summaries.push(VersionSummary {
                version: version.clone(),
                status,
                platforms_pushed,
                platforms_failed,
                cascade_tags_written: cascade_tags,
                test_failures: all_test_failures,
                platforms_excluded: collect_excluded_platforms(spec, version),
                layer_reuse,
            });
        }

        self.finalize_run(spec, version_summaries, push_job_url).await
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Point `:latest` at one already-published env entry, returning whether the
/// alias landed.
///
/// `ocx package push --cascade` derives the rolling tags by parsing the version
/// as `X.Y.Z`, so a PEP 440 version it cannot parse (`0.0.0.2`) never gets
/// `latest` and a bare reference (`repo` → `repo:latest`) stays unresolvable.
/// This re-pushes the newest version's green entries under the literal tag —
/// content-addressed, so it costs a verify plus a tag write, and each entry
/// merges into the one `latest` image index.
///
/// Best-effort by construction: the primary publish already succeeded, so a
/// failed alias warns instead of redding the version. For the same reason it
/// gets a SINGLE attempt through [`push_once`] rather than the retry ladder —
/// same precedent as `patch::republish`. A missed alias is corrected by the
/// next run.
async fn alias_newest_as_latest(
    spec: &MirrorSpec,
    env_entry: &crate::pipeline::python_prepare::EnvEntry,
    platform: &str,
    version: &str,
    annotations: &BTreeMap<String, String>,
) -> bool {
    let latest_ref = format!("{}:latest", spec.target.reference());
    let attempt = async {
        let args = python_push::build_env_push_args(
            platform,
            &latest_ref,
            &env_entry.metadata_path,
            &env_entry.layers,
            annotations,
            false,
        )?;
        let ocx_binary = resolve_ocx_binary()?;
        push_once(&ocx_binary, &args, PUSH_TIMEOUT)
            .await
            .map_err(|failure| failure.message)
    }
    .await;

    match attempt {
        Ok(_) => true,
        Err(message) => {
            log::warn!(
                "[{}] latest alias push failed for {version}/{platform}: {message}",
                spec.name,
            );
            false
        }
    }
}

/// Whether `version` — the newest version of THIS run — is also the newest
/// version the target repository holds, i.e. whether `:latest` may be moved
/// onto it.
///
/// [`alias_newest_as_latest`] otherwise only knows newest-in-run, so a
/// backfill run (`versions.backfill: oldest-first`, or an `--exact-version`
/// republish of an old release) would re-point `:latest` at a version older
/// than what is already published — silently downgrading every consumer
/// resolving the bare reference.
///
/// Fail-safe in the direction that cannot break the registry: a tag-list read
/// that fails answers `false`, so the alias is skipped and the next run
/// corrects it. Nothing here may fail the push job — the packages are already
/// published either way.
async fn run_newest_is_registry_newest(publisher: &Publisher, spec: &MirrorSpec, version: &str) -> bool {
    let identifier = ocx_lib::oci::Identifier::new_registry(&spec.target.repository, &spec.target.registry);
    let tags = match fetch_published_tags(publisher, &identifier).await {
        Ok(tags) => tags,
        Err(error) => {
            log::warn!(
                "[{}] skipping the latest alias for {version}: could not read the published tags of {identifier}: {error}",
                spec.name,
            );
            return false;
        }
    };

    match registry_tag_newer_than(&tags, version) {
        Some(newer) => {
            log::warn!(
                "[{}] skipping the latest alias for {version}: {identifier} already holds the newer tag '{newer}'",
                spec.name,
            );
            false
        }
        None => true,
    }
}

/// The latest-alias gate's tag listing, with a test-only injection seam.
///
/// The fake-`ocx` test harness fakes the subprocess, not the in-process
/// [`Publisher`], so without the seam the alias tests would read LIVE
/// registry state — passing only while the fixture's repository happens to
/// hold nothing newer, and breaking the day it does. Tests set
/// [`LATEST_TAGS_OVERRIDE`] under [`ocx_env_lock`], same discipline as every
/// other process-global test knob.
async fn fetch_published_tags(
    publisher: &Publisher,
    identifier: &ocx_lib::oci::Identifier,
) -> Result<Vec<String>, MirrorError> {
    #[cfg(test)]
    if let Some(tags) = LATEST_TAGS_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        return Ok(tags);
    }
    target_registry::list_target_tags(publisher, identifier).await
}

/// See [`fetch_published_tags`]. `Some(tags)` is consumed by the next fetch.
#[cfg(test)]
pub(crate) static LATEST_TAGS_OVERRIDE: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

/// The first published tag that is strictly newer than `version` under
/// [`pep440_sort_key`], if any.
///
/// Rolling and canonical tags are not versions and are skipped: `latest` is
/// the very tag being decided, and `sha256.<hex>` is `ocx package push`'s
/// digest-named safety net. An unparseable version on either side leaves the
/// two unordered, which counts as "newer" — the caller then declines to move
/// the alias, which is the safe direction.
fn registry_tag_newer_than<'a>(tags: &'a [String], version: &str) -> Option<&'a str> {
    let own_key = pep440_sort_key(version);
    tags.iter()
        .map(String::as_str)
        .filter(|tag| *tag != "latest" && !tag.starts_with("sha256."))
        .find(|tag| {
            let key = pep440_sort_key(tag);
            key.0.is_some() && key > own_key
        })
}

/// Total-order sort key for a PEP 440 version string:
/// `(parsed version, original text)`.
///
/// `None` sorts before `Some`, so an unparseable tag lands first and the
/// newest parseable version is LAST — which is what every "newest = last
/// element" reader here relies on. The text tiebreaks equal parses so the key
/// is a total order on distinct strings.
///
/// It replaces a pairwise comparator of the shape
/// `match (parse(a), parse(b)) { (Some, Some) => semver, _ => text }`, which is
/// not transitive and therefore not a valid `sort_by` predicate: with
/// `"10.0.0"`, `"3.0.0"` and `"2.0rc1"` (the last unparseable by
/// `ocx_lib::Version`) it yields `10.0.0 > 3.0.0 > 2.0rc1 > 10.0.0` — a cycle,
/// for which `slice::sort_by` documents an unspecified order and permits a
/// panic. Here that order decides push order and which version `:latest`
/// lands on.
///
/// `uv_pep440` rather than `ocx_lib::package::version::Version`: upstream
/// Python versions are PEP 440 (`0.0.0.2`, `2.0.0.dev0`), which the ≤3-component
/// OCX parser rejects. The `Version::parse` check that decides `--cascade`
/// stays as it is — that one asks a different question ("can ocx derive
/// rolling tags from this?").
pub(crate) fn pep440_sort_key(version: &str) -> (Option<ocx_python::uv_pep440::Version>, String) {
    (version.parse().ok(), version.to_string())
}

// ── Backfill cascade repair ──────────────────────────────────────────────────

/// The entries of a version's published index whose cascade never ran.
///
/// `ocx package push --cascade` merges only the pushed leg's OWN platform into
/// each rolling tag, so both phase-2 loops give it to every leg of a whole
/// version. That is complete for a version published in one run, and silently
/// incomplete for one completed across two: the run that first published the
/// version withheld `--cascade` from its green legs precisely because the
/// version was still partial, and the run that backfills the missing platform
/// no longer carries those legs at all — `pipeline plan` trims already-published
/// `(version, platform)` tiles (`filter::filter_versions`,
/// `plan::build_env_plan_entries`). Nothing ever cascades them, so `X.Y`, `X`
/// and `latest` end up holding the backfilled platform alone while the exact
/// version tag is correct.
///
/// A pushed platform string that does not parse excludes nothing: re-pushing an
/// entry this run already pushed is idempotent, while dropping one that still
/// needs the cascade is the bug.
fn entries_awaiting_cascade<'a>(
    published: &'a [target_registry::PublishedImage],
    platforms_pushed: &[String],
) -> Vec<&'a target_registry::PublishedImage> {
    let pushed: Vec<ocx_lib::oci::Platform> = platforms_pushed
        .iter()
        .filter_map(|platform| platform.parse().ok())
        .collect();
    published
        .iter()
        .filter(|image| !pushed.contains(&image.platform))
        .collect()
}

/// Run the cascade for every entry of `version`'s merged index this run did not
/// push, so the rolling tags reflect the whole version rather than this run's
/// legs (see [`entries_awaiting_cascade`]).
///
/// Each entry is re-emitted from the registry's own descriptors — the published
/// layers by digest, the published config metadata verbatim — so the manifest
/// written is byte-identical to the one already there and the push costs a
/// config blob plus the cascade tag writes. Nothing is downloaded and no layer
/// is re-uploaded, the same mechanism `pipeline patch` publishes through.
///
/// An entry whose config bytes this build would not reproduce exactly is
/// SKIPPED rather than re-pushed: a differing config blob yields a new platform
/// manifest digest, which would orphan the digest a consumer's lock pins — a
/// worse outcome than the rolling tag this repairs.
///
/// Best-effort by construction, on the same reasoning as
/// [`alias_newest_as_latest`]: every package of the version is already
/// published, so a failed repair warns instead of redding the version. Single
/// attempt per entry — the upload is a config blob, and the retry ladder
/// `pipeline push` needs for a 350 MB tile buys nothing here.
///
/// Returns the cascade tags written, for the run summary and the announce union.
async fn cascade_backfilled_entries(
    publisher: &Publisher,
    spec: &MirrorSpec,
    version: &str,
    platforms_pushed: &[String],
    annotations: &BTreeMap<String, String>,
) -> Vec<String> {
    let identifier = ocx_lib::oci::Identifier::new_registry(&spec.target.repository, &spec.target.registry);
    let published = match published_images_for(publisher, &identifier, version).await {
        Ok(images) => images,
        Err(error) => {
            log::warn!(
                "[{}] {version}: could not read the published index to complete its cascade, so the rolling tags \
                 may carry only this run's platforms: {error}",
                spec.name,
            );
            return Vec::new();
        }
    };

    let awaiting = entries_awaiting_cascade(&published, platforms_pushed);
    if awaiting.is_empty() {
        return Vec::new();
    }

    let work_dir = match tempfile::TempDir::new() {
        Ok(dir) => dir,
        Err(error) => {
            log::warn!(
                "[{}] {version}: could not create a sidecar directory: {error}",
                spec.name
            );
            return Vec::new();
        }
    };
    let ocx_binary = match resolve_ocx_binary() {
        Ok(binary) => binary,
        Err(error) => {
            log::warn!("[{}] {version}: {error}", spec.name);
            return Vec::new();
        }
    };

    let mut tags = Vec::new();
    for image in awaiting {
        log::info!(
            "[{}] {version} ({}): re-cascading a platform an earlier run published",
            spec.name,
            image.platform,
        );
        match re_cascade_entry(
            publisher,
            &identifier,
            spec,
            image,
            annotations,
            &ocx_binary,
            work_dir.path(),
        )
        .await
        {
            Ok(written) => tags.extend(written),
            Err(message) => log::warn!(
                "[{}] {version} ({}): the rolling tags do not carry this platform — {message}",
                spec.name,
                image.platform,
            ),
        }
    }
    tags
}

/// The repair's view of what the version tag holds, with a test-only stub.
///
/// Same hazard [`fetch_published_tags`] documents, one step worse: the
/// fake-`ocx` harness fakes the subprocess, not the in-process [`Publisher`],
/// and every green version of every push test reaches this call — so without a
/// stub the unit suite would read the LIVE `ocx.sh` state its fixtures name,
/// and then *re-push* against whatever it found. A test build therefore sees an
/// empty index and skips the repair; the mechanism itself is exercised by the
/// acceptance harness against the local registry.
#[cfg(not(test))]
async fn published_images_for(
    publisher: &Publisher,
    identifier: &ocx_lib::oci::Identifier,
    version: &str,
) -> Result<Vec<target_registry::PublishedImage>, MirrorError> {
    target_registry::fetch_published_images(publisher, identifier, &[version]).await
}

/// See [`published_images_for`] — the test build's registry-free stand-in.
#[cfg(test)]
async fn published_images_for(
    _publisher: &Publisher,
    _identifier: &ocx_lib::oci::Identifier,
    _version: &str,
) -> Result<Vec<target_registry::PublishedImage>, MirrorError> {
    Ok(Vec::new())
}

/// Re-emits one published `(version, platform)` manifest with `--cascade`, so
/// the rolling tags pick up an entry an earlier run left behind. See
/// [`cascade_backfilled_entries`] for why this is safe to run against live
/// published state.
async fn re_cascade_entry(
    publisher: &Publisher,
    identifier: &ocx_lib::oci::Identifier,
    spec: &MirrorSpec,
    image: &target_registry::PublishedImage,
    annotations: &BTreeMap<String, String>,
    ocx_binary: &Path,
    work_dir: &Path,
) -> Result<Vec<String>, String> {
    let published = target_registry::fetch_published_metadata(publisher, identifier, image)
        .await
        .map_err(|error| format!("the published metadata could not be read: {error}"))?;

    // The re-push must be a no-op on the manifest. `config_bytes_match` decides
    // that from the descriptor alone: it is false exactly when this build would
    // serialize the same document differently from whatever `ocx` published it,
    // and re-pushing then rewrites the platform manifest instead of repairing a
    // tag.
    if !plan::config_bytes_match(image, &published)
        .map_err(|error| format!("the published metadata could not be compared: {error}"))?
    {
        return Err(format!(
            "its published config blob is not what this build would write, and re-pushing it would replace the \
             manifest digest {} rather than only move the rolling tags",
            image.manifest_digest,
        ));
    }

    // The sidecar is the published metadata verbatim: `ocx package push -m`
    // reads the published form since 0.5.6, and this path must reproduce the
    // registry's config bytes exactly — no authoring round-trip, no platform
    // stamp (retired; the platform travels on `-p` alone).
    let sidecar_json = serde_json::to_string_pretty(&published)
        .map_err(|error| format!("the push sidecar could not be rendered: {error}"))?;

    let sidecar = work_dir.join(format!(
        "{}-{}-metadata.json",
        image.version,
        spec::platform_slug(&image.platform),
    ));
    tokio::fs::write(&sidecar, sidecar_json)
        .await
        .map_err(|error| format!("failed to write {}: {error}", sidecar.display()))?;

    let target_ref = format!("{}:{}", spec.target.reference(), image.version);
    let args = patch_push_args(&target_ref, image, &sidecar, annotations, true)?;

    push_once(ocx_binary, &args, PUSH_TIMEOUT)
        .await
        .map(|report| report.cascade_tags_written)
        .map_err(|failure| failure.message)
}

/// The base `os/arch` half of an env entry's full wheels-key platform string
/// (`linux/amd64+libc.glibc` → `linux/amd64`). Spec lookups (`platforms:`
/// order, containers, tests) are keyed by base — a full-key lookup would miss
/// silently and fall back to `_native_`/`usize::MAX`.
fn base_platform_str(platform: &str) -> &str {
    platform.split('+').next().unwrap_or(platform)
}

/// The `libc.*` os_feature declared on an env entry's platform string, if any.
fn entry_libc_feature(platform: &str) -> Option<&str> {
    let (_, features) = platform.split_once('+')?;
    features.split('+').find(|feature| feature.starts_with("libc."))
}

/// The container IDs whose JUnit files gate one env entry, filtered by libc
/// compatibility: a featureless entry is gated by EVERY container of its base
/// platform (it claims to run on any libc, so all legs must be green); a
/// `libc.glibc` entry only by gnu containers; a `libc.musl` entry only by
/// musl (alpine) containers. A native leg (`_native_`) counts as gnu — GHA
/// runners are glibc. An empty result means no test leg covers the entry's
/// declared libc; the caller fails closed.
fn gating_container_ids_for_entry(spec: &MirrorSpec, base_platform: &str, libc: Option<&str>) -> Vec<String> {
    let containers = spec
        .platforms
        .as_ref()
        .and_then(|platforms| platforms.get(base_platform))
        .and_then(|config| config.containers.as_deref())
        .filter(|containers| !containers.is_empty());

    let Some(containers) = containers else {
        // Native leg: a glibc runner — gates featureless and glibc entries.
        return match libc {
            None | Some("libc.glibc") => vec!["_native_".to_string()],
            _ => Vec::new(),
        };
    };

    let wanted = match libc {
        None => None, // featureless: every container gates
        Some("libc.musl") => Some("musl"),
        // `libc.glibc` (any other feature namespace is rejected at spec
        // validation) gates on gnu containers.
        Some(_) => Some("gnu"),
    };
    containers
        .iter()
        .filter(|container| wanted.is_none_or(|libc| spec::infer_libc_from_image(&container.image) == libc))
        .map(|container| {
            container
                .id
                .clone()
                .unwrap_or_else(|| spec::image_to_container_id(&container.image))
        })
        .collect()
}

/// Map `(version, platform_slug)` to the canonical bundle filename and path.
///
/// Bundles are named `bundle-{V}-{platform_slug}.tar.xz` in `bundles_dir`.
fn bundle_path_for(bundles_dir: &Path, version: &str, platform_slug: &str) -> PathBuf {
    bundles_dir.join(format!("bundle-{version}-{platform_slug}.tar.xz"))
}

/// Convert `linux/amd64` → `linux_amd64` (platform string → slug).
///
/// The canonical slug, shared with `pipeline prepare` (which names the work
/// directory) and the CI renderer (which names the JUnit file).
fn platform_to_slug(platform: &str) -> String {
    spec::platform_key_slug(platform)
}

/// Derive the upstream project homepage from a mirror spec's `source:` block.
///
/// `github_release` → `https://github.com/{owner}/{repo}`. `url_index`,
/// `pylock` and `pypi` have no canonical homepage to infer here (a generated
/// JSON index, a committed lock file, or a package name whose PyPI project
/// page needs the mirror's `name` as a fallback this function does not
/// receive), so we return `None` and let the notify embed render without an
/// author link in that case.
fn compute_source_url(source: &spec::Source) -> Option<String> {
    match source {
        spec::Source::GithubRelease { owner, repo, .. } => Some(format!("https://github.com/{owner}/{repo}")),
        spec::Source::UrlIndex(_) | spec::Source::Pylock { .. } | spec::Source::Pypi { .. } => None,
    }
}

/// Commit-pinned `logo.png` URL for the running GHA workflow.
///
/// Convention: the mirror's repo carries `logo.png` at the root. Pinning to
/// the commit SHA (rather than `main`) keeps the embed thumbnail working
/// before the file lands on the default branch.
fn compute_logo_url() -> Option<String> {
    let repo = std::env::var("GITHUB_REPOSITORY")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    let sha = std::env::var("GITHUB_SHA").ok().filter(|s| !s.trim().is_empty())?;
    Some(format!("https://raw.githubusercontent.com/{repo}/{sha}/logo.png"))
}

/// Collect declared platforms whose `broken`-severity exclude entry matches
/// `version`, for visibility (🔒 rows in the Discord report).
///
/// `skip`-severity excludes — and `min_version`/`max_version` windows — stay
/// silent (they never reach this point with a matching entry). Sorted by
/// platform for deterministic output. The excluded pairs were never built, so
/// they never overlap with `platforms_pushed` / `platforms_failed`.
fn collect_excluded_platforms(spec: &MirrorSpec, version: &str) -> Vec<ExcludedPlatform> {
    let Some(platforms) = &spec.platforms else {
        return Vec::new();
    };
    let mut excluded: Vec<ExcludedPlatform> = platforms
        .keys()
        .filter_map(|platform| {
            let entry = spec.exclude_hit(version, platform)?;
            (entry.severity == Severity::Broken).then(|| ExcludedPlatform {
                platform: platform.clone(),
                reason: entry.reason.clone(),
            })
        })
        .collect();
    excluded.sort_by(|a, b| a.platform.cmp(&b.platform));
    excluded
}

/// Returns platforms in spec declaration order.
fn spec_platform_order(spec: &MirrorSpec) -> Vec<String> {
    // IndexMap preserves insertion order; HashMap does not. The spec `platforms`
    // field is a `HashMap<String, PlatformConfig>`. We sort alphabetically as a
    // deterministic fallback when declaration order is not preserved.
    let Some(platforms) = &spec.platforms else {
        return Vec::new();
    };
    let mut keys: Vec<String> = platforms.keys().cloned().collect();
    keys.sort();
    keys
}

/// Returns the container IDs expected for a platform.
///
/// Container mode → slugified image names (`:` and `/` replaced by `_`).
/// Native mode → single entry `_native_`.
fn container_ids_for_platform(spec: &MirrorSpec, platform_str: &str) -> Vec<String> {
    let Some(platforms) = &spec.platforms else {
        return vec!["_native_".to_string()];
    };

    let Some(config) = platforms.get(platform_str) else {
        return vec!["_native_".to_string()];
    };

    container_ids_from_config(config)
}

fn container_ids_from_config(config: &PlatformConfig) -> Vec<String> {
    match &config.containers {
        None => vec!["_native_".to_string()],
        Some(containers) if containers.is_empty() => vec!["_native_".to_string()],
        Some(containers) => containers
            .iter()
            .map(|c| {
                c.id.clone().unwrap_or_else(|| {
                    // Default slug: image with `:` and `/` replaced by `_`.
                    crate::spec::image_to_container_id(&c.image)
                })
            })
            .collect(),
    }
}

/// Returns the test names declared for a platform (platform-level override or top-level).
fn test_names_for_platform(spec: &MirrorSpec, platform_str: &str) -> Vec<String> {
    // Check for platform-level test override first.
    if let Some(platforms) = &spec.platforms
        && let Some(config) = platforms.get(platform_str)
        && let Some(platform_tests) = &config.tests
    {
        return platform_tests.iter().map(|t| t.name.clone()).collect();
    }

    // Fall back to top-level tests.
    spec.tests
        .as_ref()
        .map(|tests| tests.iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default()
}

/// Evaluate the JUNIT files for a `(version, platform)` pair across all
/// declared container IDs, returning a go/no-go decision.
///
/// Takes the platform in slash form and slugs it here: reporting a failure
/// needs the platform, finding the file needs the slug, and the slug does not
/// reverse for a platform carrying `os.features`.
///
/// AND-logic: all containers must be green for all declared tests.
async fn evaluate_junit(
    junit_dir: &Path,
    version: &str,
    platform: &str,
    container_ids: &[String],
    declared_test_names: &[String],
) -> VpDecision {
    let platform_slug = platform_to_slug(platform);
    let mut platform_test_failures: Vec<TestFailure> = Vec::new();
    let mut missing_reasons: Vec<String> = Vec::new();
    // Capture the first `ci.job.url` we encounter across all containers in this
    // leg. Every container in the matrix leg shares the same matrix-leg job
    // URL, so first-non-empty wins.
    let mut job_url: Option<String> = None;

    for container_id in container_ids {
        let junit_path = junit_dir.join(format!("junit-{version}-{platform_slug}-{container_id}.xml"));

        if !junit_path.exists() {
            missing_reasons.push(format!("missing junit for container {container_id}"));
            continue;
        }

        // Parse the JUNIT file asynchronously.
        let suite = match junit::parse_async(&junit_path).await {
            Ok(s) => s,
            Err(e) => {
                missing_reasons.push(format!("parse error for {container_id}: {e}"));
                continue;
            }
        };

        if job_url.is_none() {
            job_url = suite
                .properties
                .get("ci.job.url")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
        }

        // Check suite-level failure/error counts first for efficiency.
        let suite_has_failures = suite.failures > 0 || suite.errors > 0;

        // Find all failing testcases.
        let failures_in_suite: Vec<&JunitTestcase> = suite
            .testcases
            .iter()
            .filter(|tc| tc.failure_message.is_some())
            .collect();

        for failing_tc in &failures_in_suite {
            platform_test_failures.push(TestFailure {
                version: version.to_string(),
                platform: platform.to_string(),
                container: container_id.clone(),
                test: failing_tc.name.clone(),
                message: failing_tc.failure_message.clone().unwrap_or_default(),
            });
        }

        // If suite counts indicate failures but no explicit testcase had a
        // failure_message, still treat it as failed.
        if suite_has_failures && failures_in_suite.is_empty() {
            platform_test_failures.push(TestFailure {
                version: version.to_string(),
                platform: platform.to_string(),
                container: container_id.clone(),
                test: "<suite>".to_string(),
                message: format!(
                    "testsuite reports {} failure(s) and {} error(s)",
                    suite.failures, suite.errors
                ),
            });
        }

        // Check that every declared test name is present in the JUNIT.
        if !declared_test_names.is_empty() {
            let found_names: std::collections::HashSet<&str> =
                suite.testcases.iter().map(|tc| tc.name.as_str()).collect();
            for expected_name in declared_test_names {
                if !found_names.contains(expected_name.as_str()) {
                    platform_test_failures.push(TestFailure {
                        version: version.to_string(),
                        platform: platform.to_string(),
                        container: container_id.clone(),
                        test: expected_name.clone(),
                        message: format!("test '{expected_name}' not found in JUNIT"),
                    });
                }
            }
        }
    }

    // Missing JUNIT files count as failures.
    if !missing_reasons.is_empty() {
        let reason = missing_reasons.join("; ");
        let failure = PlatformFailure {
            platform: platform.to_string(),
            reason: "missing_junit".to_string(),
            failed_tests: vec![],
            job_url: job_url.clone(),
        };
        return VpDecision::Red {
            platform_failure: failure,
            test_failures: vec![TestFailure {
                version: version.to_string(),
                platform: platform.to_string(),
                container: "_missing_".to_string(),
                test: "<junit>".to_string(),
                message: reason,
            }],
        };
    }

    if platform_test_failures.is_empty() {
        VpDecision::Green
    } else {
        let failure = PlatformFailure {
            platform: platform.to_string(),
            reason: "test_failed".to_string(),
            failed_tests: platform_test_failures.clone(),
            job_url,
        };
        VpDecision::Red {
            platform_failure: failure,
            test_failures: platform_test_failures,
        }
    }
}

/// Convert a bundle's platform slug back to its platform string
/// (`linux_amd64` → `linux/amd64`).
///
/// The spec's declared keys are consulted first, because the slug is lossy
/// wherever a platform carries `os.features`: `linux_amd64_libc.musl` has no
/// textual reversal to `linux/amd64+libc.musl`, and guessing `linux/amd64_libc.musl`
/// misses every subsequent `spec.platforms` lookup (container ids, test names)
/// and would hand `ocx package push` an unparseable `--platform`.
///
/// The `_`-splitting heuristic stays as the fallback for a bundle whose platform
/// the spec never declared under `platforms:`.
fn slug_to_platform(spec: &MirrorSpec, slug: &str) -> String {
    if let Some(platforms) = &spec.platforms
        && let Some(key) = platforms.keys().find(|key| platform_to_slug(key) == slug)
    {
        return key.clone();
    }
    slug_to_platform_heuristic(slug)
}

/// Best-effort textual reversal — replaces the first `_` that separates the OS
/// from the architecture. Known OS prefixes: `linux`, `darwin`, `windows`.
fn slug_to_platform_heuristic(slug: &str) -> String {
    for os in &["linux", "darwin", "windows"] {
        let prefix = format!("{os}_");
        if slug.starts_with(prefix.as_str()) {
            let arch = &slug[prefix.len()..];
            return format!("{os}/{arch}");
        }
    }
    // Fallback: replace first `_` with `/`.
    if let Some(pos) = slug.find('_') {
        let mut s = slug.to_string();
        s.replace_range(pos..pos + 1, "/");
        return s;
    }
    slug.to_string()
}

/// Determine the `VersionStatus` for a version based on push outcomes.
///
/// A verdict, not a tag rewriter. `cascade_tags_written` records what the
/// registry actually received; editing it here would make `run-summary.json`
/// and the Discord report describe a registry state that does not exist. A
/// `Partial` version carries only its exact `X.Y.Z` because the push loop
/// never gave it `--cascade`, not because anything trimmed the list.
///
/// The `is_newest` flag is informational — the `ocx package push --cascade`
/// subprocess handles `latest` tag writes internally based on cascade version
/// ordering.
fn determine_status(
    platforms_pushed: &[String],
    platforms_failed: &[PlatformFailure],
    all_skipped_existing: bool,
    _is_newest: bool,
) -> VersionStatus {
    if all_skipped_existing && platforms_pushed.is_empty() && platforms_failed.is_empty() {
        return VersionStatus::SkippedExisting;
    }

    if platforms_pushed.is_empty() && !platforms_failed.is_empty() {
        // All platforms failed.
        return VersionStatus::Failed;
    }

    if !platforms_pushed.is_empty() && platforms_failed.is_empty() {
        // All platforms pushed successfully.
        // The cascade tags are whatever the push subprocess returned. If `latest`
        // was not returned by the subprocess but should be written, the subprocess
        // handles that internally (ocx package push --cascade logic).
        // We don't inject `latest` ourselves — trust the subprocess output.
        return VersionStatus::Published;
    }

    // Mixed: some pushed, some failed.
    VersionStatus::Partial
}

/// Enumerate bundles from `bundles_dir`, returning a map of
/// `version → {platform_slug set}`.
///
/// Bundle filenames follow `bundle-{V}-{platform_slug}.tar.xz`.
async fn enumerate_bundles(bundles_dir: &Path) -> Result<HashMap<String, Vec<String>>, MirrorError> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    let mut read_dir = tokio::fs::read_dir(bundles_dir).await.map_err(|e| {
        MirrorError::TemplateError(format!(
            "failed to read bundles directory {}: {e}",
            bundles_dir.display()
        ))
    })?;

    while let Some(entry) = read_dir
        .next_entry()
        .await
        .map_err(|e| MirrorError::TemplateError(format!("failed to iterate bundles directory: {e}")))?
    {
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();

        // Parse `bundle-{V}-{platform_slug}.tar.xz`
        if let Some((version, platform_slug)) = parse_bundle_filename(&name) {
            map.entry(version.to_string())
                .or_default()
                .push(platform_slug.to_string());
        }
    }

    Ok(map)
}

/// Parse a bundle filename of the form `bundle-{V}-{platform_slug}.tar.xz`.
///
/// Returns `Some((version, platform_slug))` on success, `None` if the filename
/// does not match the expected pattern.
fn parse_bundle_filename(name: &str) -> Option<(&str, &str)> {
    let name = name.strip_prefix("bundle-")?;
    let name = name.strip_suffix(".tar.xz")?;

    // The remaining string is `{V}-{platform_slug}`. The platform slug contains
    // one `_` (e.g. `linux_amd64`). The version may contain `.` and digits.
    // Strategy: find the last `-` followed by a known platform slug prefix.
    // Known OS prefixes in slug form: `linux_`, `darwin_`, `windows_`.
    let platform_prefixes = ["linux_", "darwin_", "windows_"];
    for prefix in &platform_prefixes {
        // Find `-{prefix}` in the remaining string.
        let search = format!("-{prefix}");
        if let Some(pos) = name.rfind(search.as_str()) {
            let version = &name[..pos];
            let platform_slug = &name[pos + 1..];
            if !version.is_empty() && !platform_slug.is_empty() {
                return Some((version, platform_slug));
            }
        }
    }
    None
}

/// Build the `ocx package push` argv. Pure and unit-testable — locks the flag
/// order and the `--annotation KEY=VALUE` tail without spawning a subprocess.
///
/// `--format` is a global ocx flag and must precede the subcommand.
///
/// `layers` are positional layer references in manifest order, each either a
/// path to a built bundle (the push job) or a `sha256:<hex>.<ext>` reference to
/// a layer the registry already holds (`pipeline patch`). `metadata` names the
/// sidecar to publish; `None` lets `ocx` derive it from the first file layer,
/// which is what the push job relies on.
///
/// `cascade` decides whether this push also moves the rolling `latest` / `X` /
/// `X.Y` aliases onto the version's image index. Without it the push writes the
/// exact version tag and nothing else — the platform still merges into that
/// tag's index, so a version can be assembled platform by platform and only
/// advertised once it is whole. Who gets it is decided by the caller, once per
/// version; see the phase-2 loop in [`Push::execute`].
///
/// `--new` makes the FIRST push of a brand-new mirror succeed: a cascade push
/// lists existing tags to compute the rolling tags, but a not-yet-published
/// repository answers `tags/list` with 404 ("repository name not known").
/// `--new` tells `ocx package push` to treat that failure as an empty tag set
/// instead of aborting. It is a no-op once the repository exists (the tag
/// list then succeeds and is used), so the mirror always passes it.
pub(crate) fn build_push_args(
    platform: &str,
    target_ref: &str,
    layers: &[&str],
    metadata: Option<&Path>,
    annotations: &BTreeMap<String, String>,
    cascade: bool,
) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = ["--format", "json", "package", "push"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if cascade {
        args.push("--cascade".to_string());
    }
    args.extend(
        ["--new", "-p", platform, "-i", target_ref]
            .iter()
            .map(|s| (*s).to_string()),
    );
    if let Some(path) = metadata {
        let sidecar = path
            .to_str()
            .ok_or_else(|| format!("metadata path is not valid UTF-8: {}", path.display()))?;
        args.push("--metadata".to_string());
        args.push(sidecar.to_string());
    }
    args.extend(layers.iter().map(|layer| (*layer).to_string()));

    args.extend(crate::annotations::push_args(annotations));

    Ok(args)
}

/// How long one push attempt may run before it is killed.
///
/// A backstop against a wedged child, not a throughput expectation. `ocx` bounds
/// every registry request itself — 30s to connect, 120s without a byte read — so
/// an upload that is progressing at all satisfies those, and all this has to
/// catch is a child that hung in some way they did not see.
///
/// Sizing it for throughput instead is what made the previous 900s wrong: a
/// 350 MB tile had to sustain ~390 KiB/s to fit, far above the ~26 KiB/s floor
/// `ocx` itself tolerates on a 3 MiB chunk, so a link healthy by `ocx`'s
/// standard was killed on every attempt and the version never published.
///
/// The worst case is now large enough to matter: one tile exhausting
/// `max_retries: 3` is four attempts, four hours. That fits inside GitHub's
/// default 360-minute job limit, but two tiles doing it do not — the job
/// timeout, not this constant, is the real outer bound on a run, and it is the
/// one that fires first.
pub(crate) const PUSH_TIMEOUT: Duration = Duration::from_secs(3600);

/// First retry delay; each further attempt doubles it.
const PUSH_RETRY_BACKOFF_BASE: Duration = Duration::from_secs(1);

/// Ceiling on the doubling, so a large `max_retries` cannot park the job on
/// backoff alone. The shape of the ladder barely matters either way — a push
/// attempt costs minutes and the delay between them seconds.
const PUSH_RETRY_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// One failed push attempt: what to report, and whether trying again could
/// plausibly change the outcome.
#[derive(Debug)]
pub(crate) struct PushAttemptError {
    pub(crate) message: String,
    transient: bool,
}

/// Whether an `ocx package push` exit code is one this pipeline will try again.
///
/// `ocx` 0.5.3 draws the line for us: 75 means the same command may succeed if
/// it is run again (registry connect failure, timeout, rate limit), and 69
/// means rerunning will not change the outcome. Only 75 is worth an upload.
///
/// A registry denial never reaches either code — 403 is 80 (auth), which is
/// deterministic and not retried. `None` (signal-killed) is not retried either:
/// the signal came from outside, and the runner that sent it is usually about
/// to send another.
///
/// Exit 65 is likewise not retried, and from `ocx` 0.5.5 that is the code a
/// binary *older* than 0.5.5 answers with on every leg: it demands the
/// top-level `platform` key the sidecar no longer carries. Deliberately given
/// no version hint — unlike the exit-64 hint `pipeline cascade` emits for a
/// missing verb, 65 is the ordinary data-error code here and a version guess
/// would misdirect a genuine bad-metadata run. The floor is documented instead.
fn push_exit_is_transient(code: Option<i32>) -> bool {
    matches!(code, Some(code) if code == ExitCode::TempFail as i32)
}

/// Delay before attempt `attempt + 1`, doubling from
/// [`PUSH_RETRY_BACKOFF_BASE`] and capped at [`PUSH_RETRY_BACKOFF_MAX`].
///
/// Kept pure and un-jittered so the ladder is pinned by a table test; the
/// spread is applied by [`push_retry_delay`] at the call site.
fn push_retry_backoff(attempt: u32) -> Duration {
    PUSH_RETRY_BACKOFF_BASE
        .saturating_mul(2u32.saturating_pow(attempt.saturating_sub(1)))
        .min(PUSH_RETRY_BACKOFF_MAX)
}

/// `delay` spread by ±10%.
///
/// The herd this breaks up is not the one inside a run — pushes there are
/// strictly sequential — but the one across repositories: dozens of mirrors run
/// scheduled workflows against the same registry, so a rate limit or an outage
/// starts all of their ladders at the same instant and an undithered ladder
/// keeps them in lockstep for every retry after. Same ±10% default
/// go-containerregistry and oras-go ship, each despite being sequential too.
///
/// The clock's nanoseconds are the entropy. The spread only has to be
/// uncorrelated between processes, which is a far weaker property than
/// randomness, and it costs no dependency.
fn jitter(delay: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    delay.saturating_mul(90 + nanos % 21) / 100
}

/// What [`invoke_push`] actually sleeps before attempt `attempt + 1`.
///
/// Scaled down by a thousand under `cfg(test)`: the retry tests drive the ladder
/// through [`Push::execute`], a clap struct with no seam to hand a shorter base
/// in, and four real seconds of sleeping on every `task rust:verify` buys
/// nothing that [`push_retry_backoff`]'s own table test does not already pin.
/// The scaling preserves the ladder's shape; what no test then covers is the
/// production base reaching this call, which is one constant.
fn push_retry_delay(attempt: u32) -> Duration {
    let delay = jitter(push_retry_backoff(attempt));
    #[cfg(test)]
    let delay = delay / 1000;
    delay
}

/// One `ocx package push [--cascade] -p {platform} -i {target_ref} {bundle}
/// --format json` subprocess, bounded by `timeout`.
///
/// `timeout` is a parameter rather than the constant read directly so the bound
/// itself can be tested without an hour-long test.
pub(crate) async fn push_once(
    ocx_binary: &Path,
    args: &[String],
    timeout: Duration,
) -> Result<PushReport, PushAttemptError> {
    let mut cmd = tokio::process::Command::new(ocx_binary);
    cmd.args(args);

    // Forward OCX_* environment variables into the subprocess.
    // This preserves offline mode, remote mode, registry config, etc.
    forward_ocx_env(&mut cmd);

    // Tokio leaves a child running when its future is dropped; on timeout that
    // would orphan a push still streaming a bundle at the registry — and the
    // retry would then race it.
    cmd.kill_on_drop(true);

    let output = match tokio::time::timeout(timeout, cmd.output()).await {
        Err(_) => {
            return Err(PushAttemptError {
                message: format!("ocx package push timed out after {}s", timeout.as_secs()),
                transient: true,
            });
        }
        Ok(Err(e)) => {
            return Err(PushAttemptError {
                message: format!("failed to spawn ocx: {e}"),
                transient: false,
            });
        }
        Ok(Ok(output)) => output,
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(PushAttemptError {
            message: format!("ocx package push exited {}: {}", output.status, stderr.trim()),
            transient: push_exit_is_transient(output.status.code()),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).map_err(|e| PushAttemptError {
        // A push that exited 0 did its work; an unreadable report is this
        // pipeline disagreeing with that `ocx` about a format, which a second
        // run reproduces exactly.
        message: format!("failed to parse push JSON output: {e}\nstdout: {}", stdout.trim()),
        transient: false,
    })
}

/// Invoke `ocx package push [--cascade] -p {platform} -i {target_ref} {bundle} --format json`
/// as a subprocess and parse the JSON output, retrying transient failures up to
/// `concurrency.max_retries` times.
///
/// Returns the parsed `PushReport` on success, or a descriptive error string
/// on subprocess failure (caller records as `push_error` without aborting).
async fn invoke_push(
    spec: &MirrorSpec,
    platform: &str,
    target_ref: &str,
    bundle_path: &Path,
    cascade: bool,
) -> Result<PushReport, String> {
    let ocx_binary = resolve_ocx_binary()?;

    let bundle = bundle_path
        .to_str()
        .ok_or_else(|| format!("bundle path is not valid UTF-8: {}", bundle_path.display()))?;
    let annotations = crate::annotations::build_annotations(&spec.annotations);
    let args = build_push_args(platform, target_ref, &[bundle], None, &annotations, cascade)?;

    push_with_retry(
        &ocx_binary,
        &args,
        spec.concurrency.max_retries,
        &spec.name,
        target_ref,
        platform,
    )
    .await
}

/// Run one push argv to a verdict: attempt it, and retry a transient failure
/// (`ocx package push` exit 75 only) up to `budget` further times with
/// [`push_retry_delay`] between attempts.
///
/// Shared by both publish paths — the archive leg via [`invoke_push`], the env
/// leg via `pipeline::python_push::invoke_env_push` — so the ladder, the
/// transience predicate and the operator-facing wording exist once. `label` is
/// the mirror name that prefixes every line; `target_ref` and `platform` name
/// the leg in them.
///
/// Returns the parsed [`PushReport`] on success, or a descriptive error string
/// (caller records it as `push_error` without aborting the run).
pub(crate) async fn push_with_retry(
    ocx_binary: &Path,
    args: &[String],
    budget: u32,
    label: &str,
    target_ref: &str,
    platform: &str,
) -> Result<PushReport, String> {
    // The budget, named in every line this loop emits: an operator reading a
    // give-up message has to be able to tell an exhausted ladder from an exit
    // code that was never going to be retried, and to find the knob either way.
    let total = budget.saturating_add(1);
    let mut attempt = 1u32;
    loop {
        match push_once(ocx_binary, args, PUSH_TIMEOUT).await {
            Ok(report) => return Ok(report),
            Err(failure) => {
                if !failure.transient {
                    return Err(format!(
                        "{} — this exit code is not retried, whatever concurrency.max_retries ({budget}) grants",
                        failure.message,
                    ));
                }
                if attempt >= total {
                    return Err(format!(
                        "{} — gave up after {total} attempt(s); raise concurrency.max_retries ({budget}) to grant more",
                        failure.message,
                    ));
                }
                let backoff = push_retry_delay(attempt);
                // `{:?}` rather than whole seconds: the delay is jittered, so
                // the first retry lands just under or over a second and
                // `as_secs()` reported half of them as "0s".
                log::warn!(
                    "[{label}] push attempt {attempt}/{total} for {target_ref} ({platform}) failed, retrying in {backoff:?}: {}",
                    failure.message,
                );
                tokio::time::sleep(backoff).await;
                attempt += 1;
            }
        }
    }
}

/// GitHub Actions secret carrying the token `ocx package announce` uses to
/// push the fork branch and open the index pull request.
pub(crate) const ENV_ANNOUNCE_TOKEN: &str = "OCX_ANNOUNCE_TOKEN";

/// The configured announce token, or `None` when the secret is absent or blank.
///
/// A repository without it is a valid configuration — forks and test repos —
/// so every caller degrades on `None` rather than failing: the packages are in
/// the registry either way, and an announce that was never attempted must not
/// red a run that published exactly what it was asked to.
pub(crate) fn announce_token() -> Option<String> {
    std::env::var(ENV_ANNOUNCE_TOKEN).ok().filter(|t| !t.trim().is_empty())
}

/// Tags this run should announce: the union of `cascade_tags_written` across
/// every version that actually published, in run order, deduped.
///
/// This announces exactly what the registry received, and nothing is filtered
/// out here. It can be, because a rolling alias can no longer reach a partial
/// version in the first place: the phase-2 loop in [`Push::execute`] gives
/// `--cascade` to every push of a whole version and to none of a version any
/// part of which failed, so a
/// `Partial` version's `cascade_tags_written` only ever holds its exact
/// `X.Y.Z`. Filtering aliases *here* was a protection that could not work —
/// `--tags-from-file` is additive, and `ocx package announce` re-observes every tag
/// already curated on the entry, so an alias an earlier run committed is
/// re-fetched from the registry and re-committed against whatever it points at
/// now. Withholding an alias only ever blocks its first addition, and every
/// established mirror already has all of them.
///
/// Deduping is load-bearing, not cosmetic — each platform's push report
/// re-lists the same cascade hierarchy, and consecutive versions share the
/// rolling tags.
///
/// A version that only skipped-existing or only failed contributes nothing:
/// its tags are either already announced or were never written.
fn announce_tag_union(versions: &[VersionSummary]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    versions
        .iter()
        .filter(|vs| {
            matches!(vs.status, VersionStatus::Published | VersionStatus::Partial) && !vs.platforms_pushed.is_empty()
        })
        .flat_map(|vs| vs.cascade_tags_written.iter())
        .filter(|tag| seen.insert((*tag).clone()))
        .cloned()
        .collect()
}

/// Where `ocx package announce` takes its tag set from.
///
/// Both variants are **additive** — neither can remove a tag the index already
/// commits, and yank markers survive. The third mode `ocx package announce`
/// offers, `--tags`, *replaces* the curated set; a mirror must never use it,
/// because one run publishing one new version would delete every previously
/// announced version from the index entry.
pub(crate) enum TagSource<'a> {
    /// This run's own tags, handed over in a file. The pipeline's normal mode:
    /// it announces exactly what the run published and nothing else.
    File { path: &'a Path, tags: &'a [String] },
    /// Every tag the physical repository currently holds, listed by `ocx`
    /// itself. Used to catch up a mirror that published before it had an
    /// `announce:` block, where no single run's tag set can ever cover the
    /// backlog.
    FromRegistry,
}

/// Build the `ocx package announce` argv. Pure and unit-testable — locks the
/// flag set without spawning a subprocess.
///
/// `out` writes the rebuilt entry to a directory instead of opening a pull
/// request — `--out` and `--fork` are mutually exclusive on the `ocx` side, so
/// exactly one of them is emitted.
fn build_announce_args(
    config: &AnnounceConfig,
    source: &TagSource<'_>,
    out: Option<&Path>,
) -> Result<Vec<String>, String> {
    // Global flags precede the subcommand. JSON because the caller has to
    // read what the announce *did* — its exit code is 0 either way.
    let mut args: Vec<String> = ["--format", "json", "package", "announce", "--package", &config.package]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    match source {
        TagSource::File { path, .. } => {
            let file = path
                .to_str()
                .ok_or_else(|| format!("announce tags file path is not valid UTF-8: {}", path.display()))?;
            args.push("--tags-from-file".to_string());
            args.push(file.to_string());
        }
        TagSource::FromRegistry => args.push("--tags-from-registry".to_string()),
    }

    match out {
        Some(directory) => {
            let dir = directory
                .to_str()
                .ok_or_else(|| format!("announce output directory is not valid UTF-8: {}", directory.display()))?;
            args.push("--out".to_string());
            args.push(dir.to_string());
        }
        None => {
            args.push("--fork".to_string());
            args.push(config.fork.clone());
        }
    }

    args.push("--index-repo".to_string());
    args.push(config.index_repo.clone());

    Ok(args)
}

/// How long the announce subprocess may run before it is killed.
///
/// It pushes a fork branch, calls the pull-request API and observes the
/// registry — network work with no bound of its own. Unbounded, one stalled
/// call (a registry 429 retry loop is enough) takes the whole job down with it
/// on the runner timeout, and everything the run published downstream of the
/// summary write goes unreported.
pub(crate) const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(600);

/// Make this run's single index announce.
///
/// Returns `None` only when there is no `announce:` block at all. A configured
/// mirror that published nothing still reports
/// [`AnnounceOutcome::NothingToAnnounce`] — the two states have very different
/// fixes, and conflating them makes an owner who just added `announce:` to an
/// already-complete mirror read permanent silence as success.
///
/// Never returns `Err`: an announce failure is recorded in the run summary and
/// leaves the push exit code alone, because the packages are already in the
/// registry either way.
///
/// `token` is the resolved `OCX_ANNOUNCE_TOKEN` and `ocx_binary` the `ocx` to
/// drive — both passed in rather than read here so tests can exercise the
/// subprocess boundary without mutating process environment.
async fn run_announce(
    config: Option<&AnnounceConfig>,
    versions: &[VersionSummary],
    tags_file: &Path,
    token: Option<&str>,
    ocx_binary: &Path,
) -> Option<AnnounceOutcome> {
    let config = config?;
    let tags = announce_tag_union(versions);
    if tags.is_empty() {
        log::info!("[announce] {} — nothing new to announce in this run", config.package);
        return Some(AnnounceOutcome::NothingToAnnounce {
            package: config.package.clone(),
        });
    }

    if token.is_none() {
        // A mirror repo without the secret is a valid configuration, so this
        // degrades rather than failing. It must still be visible: a run that
        // pushed and then did not announce cannot read like one that did.
        println!(
            "::notice title=Index announce skipped::No {ENV_ANNOUNCE_TOKEN} secret — \
             {} published {} tag(s) but the index was not updated.",
            config.package,
            tags.len()
        );
        return Some(AnnounceOutcome::SkippedNoCredential {
            package: config.package.clone(),
        });
    }

    let source = TagSource::File {
        path: tags_file,
        tags: &tags,
    };
    match invoke_announce(config, &source, None, ocx_binary, ANNOUNCE_TIMEOUT).await {
        // `unchanged` with no pull request is the no-op: the index already
        // carried every tag this run published. Recording it as `announced`
        // is what let a run that did nothing read as one that curated tags.
        Ok(report) if report.status == "unchanged" && report.pull_request_url.is_none() => {
            log::info!("[announce] {} — index already current, nothing changed", config.package);
            Some(AnnounceOutcome::AlreadyCurrent {
                package: config.package.clone(),
            })
        }
        Ok(report) => {
            log::info!(
                "[announce] {} → {} ({} tag(s), {})",
                config.package,
                config.index_repo,
                tags.len(),
                report.pull_request_url.as_deref().unwrap_or("no pull request reported"),
            );
            Some(AnnounceOutcome::Announced {
                package: config.package.clone(),
                tags,
                pull_request_url: report.pull_request_url,
            })
        }
        Err(error) => {
            log::warn!("[announce] {} failed: {error}", config.package);
            println!("::warning title=Index announce failed::{}: {error}", config.package);
            Some(AnnounceOutcome::Failed {
                package: config.package.clone(),
                error,
            })
        }
    }
}

/// Run `ocx package announce`, materialising the tags file first when `source`
/// carries one.
///
/// `timeout` is a parameter rather than a constant read so the bound itself can
/// be tested without a ten-minute test.
pub(crate) async fn invoke_announce(
    config: &AnnounceConfig,
    source: &TagSource<'_>,
    out: Option<&Path>,
    ocx_binary: &Path,
    timeout: Duration,
) -> Result<AnnounceReport, String> {
    let args = build_announce_args(config, source, out)?;

    if let TagSource::File { path, tags } = source {
        // The tags file is a sibling of `--write-summary`, and the announce runs
        // before the summary is written — so with `--write-summary out/x.json` and
        // no `out/` yet, nothing has created the directory. Same treatment as
        // `write_run_summary`. An empty parent (a bare relative path) is a no-op.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("failed to create announce tags directory {}: {e}", parent.display()))?;
        }

        tokio::fs::write(path, tags.join("\n"))
            .await
            .map_err(|e| format!("failed to write announce tags file {}: {e}", path.display()))?;
    }

    let mut cmd = tokio::process::Command::new(ocx_binary);
    cmd.args(&args);
    forward_ocx_env(&mut cmd);
    // Tokio leaves a child running when its future is dropped; on timeout that
    // would orphan an announce still talking to the registry.
    cmd.kill_on_drop(true);

    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("ocx package announce timed out after {}s", timeout.as_secs()))?
        .map_err(|e| format!("failed to spawn ocx: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "ocx package announce exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).map_err(|e| {
        // An unreadable report is a genuine unknown, not a success: the run
        // cannot tell whether the index moved. Recording it as `failed` fails
        // the push job, which is the honest outcome — the images are live and
        // the index state is undetermined.
        format!(
            "ocx package announce reported no readable JSON ({e}): {}",
            stdout.trim()
        )
    })
}

/// The subset of `ocx package announce --format json` this pipeline reads.
///
/// `status` is `"updated"` or `"unchanged"`. `unchanged` does **not** imply no
/// pull request: an announce whose branch is ahead of the index base ensures
/// one without committing anything, and those tags are as pending as a fresh
/// run's. Only `unchanged` *and* no pull request means nothing happened.
///
/// `written_paths` is populated only in `--out` mode — the root plus one object
/// per distinct curated tag, which is the file set a real run would commit. The
/// `--fork` path returns it empty by construction, so it is the *dry run's*
/// only quantitative fact and never the real run's.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct AnnounceReport {
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) pull_request_url: Option<String>,
    #[serde(default)]
    pub(crate) written_paths: Vec<String>,
    #[serde(default)]
    pub(crate) reserved_tags_dropped: Vec<String>,
}

/// Write a [`RunSummary`] to the given path as pretty-printed JSON.
async fn write_run_summary(path: &Path, summary: &RunSummary) -> Result<(), MirrorError> {
    let json = serde_json::to_string_pretty(summary)
        .map_err(|e| MirrorError::RunSummaryError(format!("failed to serialize run-summary: {e}")))?;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            MirrorError::TemplateError(format!("failed to create summary directory {}: {e}", parent.display()))
        })?;
    }

    tokio::fs::write(path, &json)
        .await
        .map_err(|e| MirrorError::TemplateError(format!("failed to write run-summary to {}: {e}", path.display())))?;

    Ok(())
}

/// Serialises every test in this crate that reads or writes the process-global
/// `OCX_*` environment — `OCX_BINARY_PIN` above all.
///
/// One lock, not one per test module: the hazard is a *neighbouring* module's
/// stub. A `pipeline plan` pypi test pinning `OCX_BINARY_PIN` at its `uv`
/// stand-in while a `pipeline push` test assumes "no `ocx` is reachable" makes
/// the push resolve that stand-in and publish into another test's fixture — a
/// failure that reproduces roughly one run in twelve and never in isolation.
///
/// `tokio::sync::Mutex` rather than `std::sync::Mutex`: `lock_derive`'s
/// `#[tokio::test]`s must hold the guard across their subprocess `.await`s
/// (async-aware guard, no `await_holding_lock`), while this module's and
/// `plan`'s sync `#[test]`s take it via [`ocx_env_lock`]'s `blocking_lock`
/// *before* entering their `Runtime::block_on`. It is not reentrant, so it is
/// taken by the test, never by a helper.
#[cfg(test)]
pub(crate) static OCX_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Blocking accessor for [`OCX_ENV_LOCK`] — sync `#[test]` contexts only
/// (`blocking_lock` panics inside a runtime; async tests lock the static
/// directly with `.lock().await`).
#[cfg(test)]
pub(crate) fn ocx_env_lock() -> tokio::sync::MutexGuard<'static, ()> {
    OCX_ENV_LOCK.blocking_lock()
}

#[cfg(test)]
#[path = "push/tests.rs"]
mod tests;
