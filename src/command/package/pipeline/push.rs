// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline push` — aggregate JUNIT results, apply go/no-go logic,
//! call `ocx package push --cascade --format json` for passing `(V, P)` pairs,
//! and emit `run-summary.json`.

mod alias;
mod bundles;
mod gating;
mod verdict;

// Glob, as in the ci renderer: the `#[path]` test modules reach this one
// through `use super::super::*;`, so a glob keeps each child's surface
// visible to them without naming items push.rs does not itself call.
use alias::*;
use bundles::*;
use gating::*;
use verdict::*;

use std::path::{Path, PathBuf};

use ocx_lib::cli::DataInterface;
use ocx_lib::log;
use ocx_lib::oci::ClientBuilder;
use ocx_lib::package::version::Version;
use ocx_lib::publisher::Publisher;

use crate::error::MirrorError;
use crate::filter::pep440_sort_key;
use crate::pipeline::ocx_cli::announce::{
    ANNOUNCE_TIMEOUT, ENV_ANNOUNCE_TOKEN, TagSource, announce_token, invoke_announce,
};
use crate::pipeline::ocx_cli::push::{PushReport, build_push_args, push_with_retry};
use crate::pipeline::ocx_cli::resolve_ocx_binary;
use crate::pipeline::python_prepare::EnvManifest;
use crate::pipeline::python_push;
use crate::run_summary::{
    AnnounceOutcome, LayerReuse, PlatformFailure, RunSummary, TestFailure, VersionStatus, VersionSummary,
};
use crate::spec::{self, AnnounceConfig, MirrorSpec};

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

// ── Backfill cascade repair ──────────────────────────────────────────────────

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

#[cfg(test)]
#[path = "push/tests.rs"]
mod tests;
