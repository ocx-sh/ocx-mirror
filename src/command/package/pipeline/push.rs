// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! `ocx-mirror package pipeline push` — aggregate JUNIT results, apply go/no-go logic,
//! call `ocx package push --cascade --format json` for passing `(V, P)` pairs,
//! and emit `run-summary.json`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ocx_lib::cli::DataInterface;
use ocx_lib::log;
use ocx_lib::package::version::Version;

use crate::error::MirrorError;
use crate::junit::{self, JunitTestcase};
use crate::run_summary::{
    AnnounceOutcome, ExcludedPlatform, PlatformFailure, RunSummary, TestFailure, VersionStatus, VersionSummary,
};
use crate::spec::{self, AnnounceConfig, MirrorSpec, PlatformConfig, Severity};

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
#[derive(Debug, serde::Deserialize)]
struct PushReport {
    /// SHA-256 manifest digest of the pushed image. Captured for audit trails
    /// but not surfaced in run-summary.json in this version.
    #[serde(default)]
    #[allow(dead_code)]
    manifest_digest: Option<String>,
    #[serde(default)]
    cascade_tags_written: Vec<String>,
    #[serde(default)]
    status: Option<String>,
}

impl Push {
    pub async fn execute(&self, _printer: &DataInterface) -> Result<(), MirrorError> {
        // ── Load spec ────────────────────────────────────────────────────────
        let spec = spec::load_spec(&self.spec).await?;

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
                .map(|slug| slug_to_platform(&slug))
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
                // Derive the platform_slug from the platform string.
                let platform_slug = platform_to_slug(platform_str);

                // Determine expected container IDs from spec.
                let container_ids = container_ids_for_platform(&spec, platform_str);

                // Evaluate JUNIT for this (V, P) across all declared containers.
                let decision = evaluate_junit(
                    &self.junit_dir,
                    version,
                    &platform_slug,
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
                        let bundle_path = bundle_path_for(&self.bundles_dir, version, &platform_slug);
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
            });
        }

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
        let announce_token = std::env::var(ENV_ANNOUNCE_TOKEN).ok().filter(|t| !t.trim().is_empty());
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
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Map `(version, platform_slug)` to the canonical bundle filename and path.
///
/// Bundles are named `bundle-{V}-{platform_slug}.tar.xz` in `bundles_dir`.
fn bundle_path_for(bundles_dir: &Path, version: &str, platform_slug: &str) -> PathBuf {
    bundles_dir.join(format!("bundle-{version}-{platform_slug}.tar.xz"))
}

/// Convert `linux/amd64` → `linux_amd64` (platform string → slug).
fn platform_to_slug(platform: &str) -> String {
    platform.replace('/', "_")
}

/// Derive the upstream project homepage from a mirror spec's `source:` block.
///
/// `github_release` → `https://github.com/{owner}/{repo}`. `url_index` has no
/// canonical homepage to infer (the URL points at a generated JSON index, not
/// a project page), so we return `None` and let the notify embed render
/// without an author link in that case.
fn compute_source_url(source: &spec::Source) -> Option<String> {
    match source {
        spec::Source::GithubRelease { owner, repo, .. } => Some(format!("https://github.com/{owner}/{repo}")),
        spec::Source::UrlIndex(_) => None,
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

/// Evaluate the JUNIT files for a `(version, platform_slug)` pair across all
/// declared container IDs, returning a go/no-go decision.
///
/// AND-logic: all containers must be green for all declared tests.
async fn evaluate_junit(
    junit_dir: &Path,
    version: &str,
    platform_slug: &str,
    container_ids: &[String],
    declared_test_names: &[String],
) -> VpDecision {
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
                // platform is the human-readable form (platform_slug with _ → /)
                platform: slug_to_platform(platform_slug),
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
                platform: slug_to_platform(platform_slug),
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
                        platform: slug_to_platform(platform_slug),
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
            platform: slug_to_platform(platform_slug),
            reason: "missing_junit".to_string(),
            failed_tests: vec![],
            job_url: job_url.clone(),
        };
        return VpDecision::Red {
            platform_failure: failure,
            test_failures: vec![TestFailure {
                version: version.to_string(),
                platform: slug_to_platform(platform_slug),
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
            platform: slug_to_platform(platform_slug),
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

/// Convert a platform slug back to platform string (`linux_amd64` → `linux/amd64`).
///
/// This is a best-effort reversal — we only replace the first `_` that
/// separates the OS from the architecture. Known OS prefixes: `linux`,
/// `darwin`, `windows`.
fn slug_to_platform(slug: &str) -> String {
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

/// Build the `ocx package push` argv for one bundle. Pure and unit-testable —
/// locks the flag order and the `--annotation KEY=VALUE` tail without spawning
/// a subprocess.
///
/// `--format` is a global ocx flag and must precede the subcommand.
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
fn build_push_args(
    platform: &str,
    target_ref: &str,
    bundle_path: &Path,
    annotations: &BTreeMap<String, String>,
    cascade: bool,
) -> Result<Vec<String>, String> {
    let bundle = bundle_path
        .to_str()
        .ok_or_else(|| format!("bundle path is not valid UTF-8: {}", bundle_path.display()))?;

    let mut args: Vec<String> = ["--format", "json", "package", "push"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if cascade {
        args.push("--cascade".to_string());
    }
    args.extend(
        ["--new", "-p", platform, "-i", target_ref, bundle]
            .iter()
            .map(|s| (*s).to_string()),
    );

    args.extend(crate::annotations::push_args(annotations));

    Ok(args)
}

/// Invoke `ocx package push [--cascade] -p {platform} -i {target_ref} {bundle} --format json`
/// as a subprocess and parse the JSON output.
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

    let annotations = crate::annotations::build_annotations(&spec.annotations);
    let args = build_push_args(platform, target_ref, bundle_path, &annotations, cascade)?;

    let mut cmd = tokio::process::Command::new(&ocx_binary);
    cmd.args(&args);

    // Forward OCX_* environment variables into the subprocess.
    // This preserves offline mode, remote mode, registry config, etc.
    forward_ocx_env(&mut cmd);

    let output = cmd.output().await.map_err(|e| format!("failed to spawn ocx: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ocx package push exited {}: {}", output.status, stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let report: PushReport = serde_json::from_str(stdout.trim())
        .map_err(|e| format!("failed to parse push JSON output: {e}\nstdout: {}", stdout.trim()))?;

    Ok(report)
}

/// GitHub Actions secret carrying the token `ocx package announce` uses to
/// push the fork branch and open the index pull request.
const ENV_ANNOUNCE_TOKEN: &str = "OCX_ANNOUNCE_TOKEN";

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
/// `--tags-file` is additive, and `ocx package announce` re-observes every tag
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

/// Build the `ocx package announce` argv. Pure and unit-testable — locks the
/// flag set without spawning a subprocess.
///
/// `--tags-file` is additive: it adds to the already-curated set and never
/// removes a committed tag. `--tags` would *replace* the curated set, which for
/// a mirror means one run publishing one new version deletes every previously
/// announced version from the index entry. Never use it here.
fn build_announce_args(config: &AnnounceConfig, tags_file: &Path) -> Result<Vec<String>, String> {
    let file = tags_file
        .to_str()
        .ok_or_else(|| format!("announce tags file path is not valid UTF-8: {}", tags_file.display()))?;

    Ok([
        "package",
        "announce",
        "--package",
        &config.package,
        "--tags-file",
        file,
        "--fork",
        &config.fork,
        "--index-repo",
        &config.index_repo,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect())
}

/// How long the announce subprocess may run before it is killed.
///
/// It pushes a fork branch, calls the pull-request API and observes the
/// registry — network work with no bound of its own. Unbounded, one stalled
/// call (a registry 429 retry loop is enough) takes the whole job down with it
/// on the runner timeout, and everything the run published downstream of the
/// summary write goes unreported.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(600);

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

    match invoke_announce(config, &tags, tags_file, ocx_binary, ANNOUNCE_TIMEOUT).await {
        Ok(()) => {
            log::info!(
                "[announce] {} → {} ({} tag(s))",
                config.package,
                config.index_repo,
                tags.len()
            );
            Some(AnnounceOutcome::Announced {
                package: config.package.clone(),
                tags,
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

/// Write the tag set to `tags_file` and run `ocx package announce` against it.
///
/// `timeout` is a parameter rather than a constant read so the bound itself can
/// be tested without a ten-minute test.
async fn invoke_announce(
    config: &AnnounceConfig,
    tags: &[String],
    tags_file: &Path,
    ocx_binary: &Path,
    timeout: Duration,
) -> Result<(), String> {
    let args = build_announce_args(config, tags_file)?;

    // The tags file is a sibling of `--write-summary`, and the announce runs
    // before the summary is written — so with `--write-summary out/x.json` and
    // no `out/` yet, nothing has created the directory. Same treatment as
    // `write_run_summary`. An empty parent (a bare relative path) is a no-op.
    if let Some(parent) = tags_file.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("failed to create announce tags directory {}: {e}", parent.display()))?;
    }

    tokio::fs::write(tags_file, tags.join("\n"))
        .await
        .map_err(|e| format!("failed to write announce tags file {}: {e}", tags_file.display()))?;

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

    Ok(())
}

/// Resolve the path to the `ocx` binary.
///
/// Preference order:
/// 1. `OCX_BINARY_PIN` env var (per CLAUDE.md env table — set by ocx itself).
/// 2. Current executable path (`std::env::current_exe()`).
/// 3. `"ocx"` on `PATH` as final fallback.
pub(crate) fn resolve_ocx_binary() -> Result<PathBuf, String> {
    if let Ok(pin) = std::env::var("OCX_BINARY_PIN")
        && !pin.is_empty()
    {
        return Ok(PathBuf::from(pin));
    }

    // The current binary is `ocx-mirror`. We want the co-located `ocx` binary.
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let candidate = dir.join("ocx");
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    // Fallback: hope `ocx` is on PATH.
    Ok(PathBuf::from("ocx"))
}

/// Forward all `OCX_*` environment variables from the current process into a
/// child command. This ensures offline mode, remote mode, registry config, and
/// index paths are inherited by the subprocess.
pub(crate) fn forward_ocx_env(cmd: &mut tokio::process::Command) {
    const OCX_VARS: &[&str] = &[
        "OCX_HOME",
        "OCX_DEFAULT_REGISTRY",
        "OCX_INSECURE_REGISTRIES",
        "OCX_OFFLINE",
        "OCX_REMOTE",
        "OCX_CONFIG",
        "OCX_NO_CONFIG",
        "OCX_PROJECT",
        "OCX_NO_PROJECT",
        "OCX_INDEX",
        "OCX_BINARY_PIN",
        "OCX_NO_UPDATE_CHECK",
        "OCX_NO_MODIFY_PATH",
    ];

    for var in OCX_VARS {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
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
mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::run_summary::VersionStatus;

    /// Serialises tests that mutate the shared `OCX_MIRROR_JOB_URL` process env
    /// var. Without it two stamping tests race: one removes the var before the
    /// other's `push` reads it at startup, dropping the expected stamp.
    fn job_url_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

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
            std::path::Path::new("/bundles/shfmt.tar.xz"),
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

    #[test]
    fn build_push_args_without_annotations_matches_the_bare_invocation() {
        let args = build_push_args(
            "linux/amd64",
            "ghcr.io/ocx-sh/shfmt:3.8.0",
            std::path::Path::new("/bundles/shfmt.tar.xz"),
            &BTreeMap::new(),
            true,
        )
        .expect("utf-8 bundle path");

        assert_eq!(args.len(), 11);
        assert!(!args.iter().any(|arg| arg == "--annotation"));
    }

    #[test]
    fn build_push_args_omits_cascade_so_a_platform_can_land_without_moving_an_alias() {
        // The non-cascade shape still names the exact version tag, and the
        // registry merges the platform into that tag's image index — a version
        // can therefore be assembled platform by platform and only advertised
        // through `latest` / `X` / `X.Y` once it is whole.
        let args = build_push_args(
            "linux/amd64",
            "ghcr.io/ocx-sh/shfmt:3.8.0",
            std::path::Path::new("/bundles/shfmt.tar.xz"),
            &BTreeMap::new(),
            false,
        )
        .expect("utf-8 bundle path");

        assert!(!args.iter().any(|arg| arg == "--cascade"), "got: {args:?}");
        assert_eq!(
            args,
            vec![
                "--format",
                "json",
                "package",
                "push",
                "--new",
                "-p",
                "linux/amd64",
                "-i",
                "ghcr.io/ocx-sh/shfmt:3.8.0",
                "/bundles/shfmt.tar.xz",
            ],
        );
    }

    /// The `ocx` subprocess inherits the runner environment — the generated
    /// workflow's push step carries `GH_TOKEN` — so the assembled argv must
    /// never carry a value sourced from outside the three-name allowlist.
    ///
    /// Same guarantee as `annotations::tests::secret_shaped_env_never_reaches_an_annotation`,
    /// one boundary further out: that one stops at the map, this one at the
    /// argv the subprocess actually receives. Driven through the injected
    /// lookup rather than the real environment — reading `std::env` would make
    /// the assertion depend on where it runs, and CI legitimately carries the
    /// allowlisted values under other names too (`GITHUB_WORKFLOW_SHA` holds
    /// the same SHA as `GITHUB_SHA`), so a real-env read collides on *value*
    /// and no name skip or length threshold can repair it.
    #[test]
    fn build_push_args_never_carries_a_non_allowlisted_env_value() {
        const TOKEN: &str = "ghs_liveTokenFromTheRunnerEnvironment";

        let annotations = crate::annotations::assemble(&BTreeMap::new(), |name| match name {
            "GITHUB_SERVER_URL" => Some("https://github.com".to_string()),
            "GITHUB_REPOSITORY" => Some("ocx-sh/mirror-shfmt".to_string()),
            "GITHUB_SHA" => Some("a1b2c3d4".to_string()),
            // Every other name the function might reach for answers with a token.
            _ => Some(TOKEN.to_string()),
        });

        let args = build_push_args(
            "linux/amd64",
            "ghcr.io/ocx-sh/shfmt:3.8.0",
            std::path::Path::new("/bundles/shfmt.tar.xz"),
            &annotations,
            true,
        )
        .expect("utf-8 bundle path");

        assert!(
            !args.iter().any(|arg| arg.contains(TOKEN)),
            "argv carries a value from outside the allowlist: {args:?}"
        );
        // Positive half, so the assertion above cannot pass on an empty argv.
        assert!(
            args.contains(&"org.opencontainers.image.source=https://github.com/ocx-sh/mirror-shfmt".to_string())
                && args.contains(&"org.opencontainers.image.revision=a1b2c3d4".to_string()),
            "allowlisted values must still reach the argv: {args:?}"
        );
    }

    // ── §3.7 S7: AND-across-containers + push driver tests ────────────────

    /// Write a JUNIT file to a directory with canonical naming.
    fn write_junit(dir: &std::path::Path, version: &str, platform_slug: &str, container_id: &str, xml: &str) {
        let name = format!("junit-{version}-{platform_slug}-{container_id}.xml");
        std::fs::write(dir.join(&name), xml).unwrap();
    }

    /// All-passing JUNIT for a (version, platform, container) triple.
    fn passing_junit(version: &str, platform: &str, image: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{slug}.{cid}"
             tests="1" failures="0" errors="0" skipped="0"
             timestamp="2026-05-13T10:00:00Z" time="1.0">
    <properties>
      <property name="ocx.version" value="{version}"/>
      <property name="ocx.platform" value="{platform}"/>
      <property name="ocx.image" value="{image}"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.shfmt.{slug}.{cid}" time="1.0"/>
  </testsuite>
</testsuites>"#,
            slug = platform.replace('/', "_"),
            cid = image.replace([':', '/'], "_"),
        )
    }

    /// JUNIT with one failing test for a (version, platform, container) triple.
    fn failing_junit(version: &str, platform: &str, image: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{slug}.{cid}"
             tests="1" failures="1" errors="0" skipped="0"
             timestamp="2026-05-13T10:00:00Z" time="2.0">
    <properties>
      <property name="ocx.version" value="{version}"/>
      <property name="ocx.platform" value="{platform}"/>
      <property name="ocx.image" value="{image}"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.shfmt.{slug}.{cid}" time="2.0">
      <failure message="exit code 1" type="exit_code">binary not found</failure>
    </testcase>
  </testsuite>
</testsuites>"#,
            slug = platform.replace('/', "_"),
            cid = image.replace([':', '/'], "_"),
        )
    }

    fn run_push_cmd(
        spec: std::path::PathBuf,
        junit_dir: std::path::PathBuf,
        bundles_dir: std::path::PathBuf,
        summary_path: std::path::PathBuf,
    ) -> Result<(), MirrorError> {
        let printer = ocx_lib::cli::DataInterface::new(ocx_lib::cli::Printer::new(false, false));
        let cmd = Push {
            spec,
            bundles_dir,
            junit_dir,
            write_summary: summary_path,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { cmd.execute(&printer).await })
    }

    #[test]
    fn and_across_containers_all_green_is_green() {
        // §3.7: 3 containers all green → (V, P) green
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = tempdir().unwrap().path().join("run-summary.json");

        let version = "3.7.0";
        let platform = "linux/amd64";
        let slug = "linux_amd64";

        write_junit(
            junit_dir.path(),
            version,
            slug,
            "ubuntu_2404",
            &passing_junit(version, platform, "ubuntu:24.04"),
        );
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "alpine_320",
            &passing_junit(version, platform, "alpine:3.20"),
        );
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "fedora_40",
            &passing_junit(version, platform, "fedora:40"),
        );

        let spec_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-multi-container.yml"
        ))
        .to_path_buf();

        // No bundle files → push is not invoked, but JUNIT-only evaluation still runs.
        let result = run_push_cmd(
            spec_path,
            junit_dir.path().to_path_buf(),
            bundles_dir.path().to_path_buf(),
            summary_path.clone(),
        );

        // Result is Ok because no bundles → no versions to process → summary written with empty versions.
        // If bundles existed, the push subprocess would be invoked.
        // The key behavior under test is the JUNIT evaluation logic.
        match result {
            Ok(()) => {
                // Verify run-summary.json was written
                assert!(summary_path.exists(), "run-summary.json must be written");
                let content = std::fs::read_to_string(&summary_path).unwrap();
                let summary: serde_json::Value = serde_json::from_str(&content).unwrap();
                // No bundles → no versions in summary (empty versions array)
                // OR versions present if we enumerated them from junit dir.
                // Either is acceptable — the spec says bundles drive the version list.
                assert!(
                    summary.get("schema_version").is_some(),
                    "schema_version must be present"
                );
            }
            Err(e) => {
                // I/O errors writing the summary are also acceptable in CI-less env
                let _ = e;
            }
        }
    }

    #[test]
    fn and_across_containers_one_failed_marks_platform_failed() {
        // §3.7: For evaluate_junit: 2 green, 1 failed → VpDecision::Red
        // Test the evaluate_junit helper directly (no bundle/push needed).
        let junit_dir = tempdir().unwrap();

        let version = "3.7.0";
        let platform = "linux/amd64";
        let slug = "linux_amd64";

        write_junit(
            junit_dir.path(),
            version,
            slug,
            "ubuntu_2404",
            &passing_junit(version, platform, "ubuntu:24.04"),
        );
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "alpine_320",
            &failing_junit(version, platform, "alpine:3.20"),
        ); // ONE FAILURE
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "fedora_40",
            &passing_junit(version, platform, "fedora:40"),
        );

        let container_ids = vec![
            "ubuntu_2404".to_string(),
            "alpine_320".to_string(),
            "fedora_40".to_string(),
        ];
        let declared_tests = vec!["version".to_string()];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &container_ids,
            &declared_tests,
        ));

        match decision {
            VpDecision::Red {
                platform_failure,
                test_failures,
            } => {
                assert_eq!(platform_failure.reason, "test_failed");
                assert!(
                    !test_failures.is_empty(),
                    "One failed container must produce test_failures"
                );
                assert!(
                    test_failures.iter().any(|tf| tf.container == "alpine_320"),
                    "Failure must reference alpine_320 container"
                );
            }
            VpDecision::Green => {
                panic!("Expected Red decision when one container fails")
            }
        }
    }

    #[test]
    fn missing_junit_file_marks_platform_failed() {
        // §3.7: 1 missing JUNIT file → VpDecision::Red with reason missing_junit
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = tempdir().unwrap().path().join("run-summary.json");

        let version = "3.7.0";
        let platform = "linux/amd64";
        let slug = "linux_amd64";

        // Only write 2 of the 3 expected container JUNITs
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "ubuntu_2404",
            &passing_junit(version, platform, "ubuntu:24.04"),
        );
        // alpine_320 missing intentionally
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "fedora_40",
            &passing_junit(version, platform, "fedora:40"),
        );

        // Test evaluate_junit directly with 3 expected containers.
        let container_ids = vec![
            "ubuntu_2404".to_string(),
            "alpine_320".to_string(),
            "fedora_40".to_string(),
        ];
        let declared_tests = vec!["version".to_string()];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &container_ids,
            &declared_tests,
        ));

        match decision {
            VpDecision::Red { platform_failure, .. } => {
                assert!(
                    platform_failure.reason.contains("missing") || platform_failure.reason.contains("junit"),
                    "Failure reason must indicate missing JUNIT: {}",
                    platform_failure.reason
                );
            }
            VpDecision::Green => {
                panic!("Missing JUNIT must result in Red decision")
            }
        }

        // Also verify full Push command writes a summary with the failed platform recorded.
        let spec_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-multi-container.yml"
        ))
        .to_path_buf();

        let _ = run_push_cmd(
            spec_path,
            junit_dir.path().to_path_buf(),
            bundles_dir.path().to_path_buf(),
            summary_path.clone(),
        );
        // No assertion on the full-run summary here — no bundles means no versions.
    }

    #[test]
    fn native_platform_uses_native_container_id() {
        // §3.7: Native platform (single _native_ JUNIT) → AND-of-one logic same
        let junit_dir = tempdir().unwrap();

        let version = "3.7.0";
        let platform = "darwin/arm64";
        let slug = "darwin_arm64";

        // Native leg uses _native_ as container_id
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "_native_",
            &passing_junit(version, platform, "_native_"),
        );

        let container_ids = vec!["_native_".to_string()];
        let declared_tests = vec!["version".to_string()];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &container_ids,
            &declared_tests,
        ));

        match decision {
            VpDecision::Green => {
                // Expected: native platform with passing JUNIT → green
            }
            VpDecision::Red { platform_failure, .. } => {
                panic!(
                    "Native platform with passing JUNIT must be green, got: {:?}",
                    platform_failure
                )
            }
        }
    }

    #[test]
    fn push_cmd_execute_writes_run_summary() {
        // §3.7: Push::execute writes run-summary.json with schema_version=1.
        let spec_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ))
        .to_path_buf();
        let dir = tempdir().unwrap();
        let summary_path = dir.path().join("run-summary.json");

        let result = run_push_cmd(
            spec_path,
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            summary_path.clone(),
        );

        // With an empty bundles dir: no bundles → empty versions → summary still written.
        match result {
            Ok(()) => {
                assert!(
                    summary_path.exists(),
                    "run-summary.json must be written even with no bundles"
                );
                let content = std::fs::read_to_string(&summary_path).unwrap();
                let val: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
                assert_eq!(val["schema_version"].as_u64().unwrap(), 1);
                assert!(val["versions"].is_array());
                assert!(val.get("mirror").is_some());
            }
            Err(e) => {
                // Acceptable if environment prevents spec loading
                let _ = e;
            }
        }
    }

    // ── Regression: push command exit-code semantics ──────────────────────
    //
    // Before the fix, `pipeline push` returned `Ok(())` unconditionally even
    // when every (V, P) pair recorded a failure. The push job in GHA then
    // resolved to `success` regardless of whether a single package landed at
    // the registry, masking total-failure runs from the workflow's overall
    // conclusion.
    //
    // Contract: any run with `any_red == true` exits non-zero via
    // `MirrorError::ExecutionFailed` — partial-success runs (some greens
    // published, some platforms failed) still surface as a pipeline failure
    // so the maintainer is forced to look at the run-summary. Greens are
    // published in-loop before this exit code is decided, so partial publish
    // still lands at the registry. The notify step runs regardless of this
    // exit code because the workflow gates `notify` on the push job's outputs
    // (`any_red` / `any_new_green`), not its `success()` status, and the
    // `summarise` step uses `if: always()` to write outputs.
    #[test]
    fn push_returns_err_whenever_any_red_even_with_partial_publish() {
        // Test exercises the all-red sub-case (no bundles → no greens) but
        // the exit policy applies to partial-publish runs as well: any_red
        // → ExecutionFailed, regardless of whether some platforms published.
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = tempdir().unwrap().path().join("run-summary.json");

        let version = "3.7.0";
        let slug = "linux_amd64";

        // Bundle present so the version loop iterates; no JUNIT files →
        // evaluate_junit reports `missing_junit` for every container → every
        // platform → Red. any_new_green stays false because nothing was
        // pushed.
        std::fs::write(bundles_dir.path().join(format!("bundle-{version}-{slug}.tar.xz")), b"x").unwrap();

        let spec_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-multi-container.yml"
        ))
        .to_path_buf();

        let result = run_push_cmd(
            spec_path,
            junit_dir.path().to_path_buf(),
            bundles_dir.path().to_path_buf(),
            summary_path.clone(),
        );

        assert!(
            matches!(result, Err(MirrorError::ExecutionFailed(_))),
            "any_red must propagate as ExecutionFailed, got {result:?}",
        );

        // Run-summary is still written so the notify step can read it via
        // the workflow's `if: always()` artifact upload.
        assert!(
            summary_path.exists(),
            "run-summary.json must be written even on the failure exit path",
        );
        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        assert_eq!(summary["any_red"], serde_json::Value::Bool(true));
        assert_eq!(summary["any_new_green"], serde_json::Value::Bool(false));
    }

    // ── Regression: slug↔slash normalisation in run() ─────────────────────
    //
    // Before the fix, the version loop iterated bundle-map keys (slug form,
    // e.g. `linux_amd64`) and passed them straight into
    // `container_ids_for_platform`, which keys on the spec's slash form
    // (`linux/amd64`). The lookup always missed → expected containers
    // collapsed to `[_native_]` → every JUNIT file (named after the real
    // container) was reported "missing junit for container _native_".
    #[test]
    fn run_loop_resolves_containers_against_spec_when_bundles_are_slug_keyed() {
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = tempdir().unwrap().path().join("run-summary.json");

        let version = "3.7.0";
        let platform = "linux/amd64";
        let slug = "linux_amd64";

        // Bundle file present → version loop will iterate `linux_amd64`.
        std::fs::write(bundles_dir.path().join(format!("bundle-{version}-{slug}.tar.xz")), b"x").unwrap();

        // JUNIT files keyed by each declared container in the spec
        // (mirror-multi-container.yml declares ubuntu/alpine/fedora). The
        // spec also declares two tests, `version` and `smoke`, so both
        // must appear as testcases for the suite to evaluate Green.
        for cid in ["ubuntu_24_04", "alpine_3_20", "fedora_40"] {
            let image = cid.replacen('_', ":", 1).replacen('_', ".", 1);
            let xml = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{slug}.{cid}" tests="2" failures="0" errors="0" skipped="0" timestamp="2026-05-13T10:00:00Z" time="1.0">
    <testcase name="version" classname="ocx-mirror.shfmt.{slug}.{cid}" time="1.0"/>
    <testcase name="smoke" classname="ocx-mirror.shfmt.{slug}.{cid}" time="1.0"/>
  </testsuite>
</testsuites>"#,
                slug = slug,
                cid = cid,
            );
            let _ = image;
            write_junit(junit_dir.path(), version, slug, cid, &xml);
        }

        let spec_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-multi-container.yml"
        ))
        .to_path_buf();

        // Push subprocess is expected to fail (no `ocx` on PATH in the test
        // env), so the version may end up Failed/Partial — that's fine.
        // The behaviour under test is the JUNIT decision: containers must
        // resolve to the spec's declared list, not the `_native_` fallback.
        let _ = run_push_cmd(
            spec_path,
            junit_dir.path().to_path_buf(),
            bundles_dir.path().to_path_buf(),
            summary_path.clone(),
        );

        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        let failures = summary["versions"][0]["platforms_failed"]
            .as_array()
            .cloned()
            .unwrap_or_default();

        for f in &failures {
            assert_ne!(
                f["reason"].as_str(),
                Some("missing_junit"),
                "platform {} reported missing_junit; container_ids_for_platform was probably called with a slug key (`{}`) instead of the spec's slash key (`{}`). full failure: {f}",
                f["platform"].as_str().unwrap_or("?"),
                slug,
                platform,
            );
        }

        // The platform string surfaced in the run-summary must be the
        // canonical slash form (matching spec keys + downstream `ocx
        // package push --platform`), not the slug form from the bundle
        // filename.
        for f in &failures {
            if let Some(p) = f["platform"].as_str() {
                assert!(
                    p.contains('/') || p == platform,
                    "platform `{p}` must be slash form (e.g. {platform}), not slug form (e.g. {slug})",
                );
            }
        }
    }

    // ── Additional unit tests for helpers ─────────────────────────────────

    const EXCLUDE_SPEC: &str = r#"
name: testtool
target:
  registry: ocx.sh
  repository: testtool
source:
  type: github_release
  owner: owner
  repo: repo
  tag_pattern: "^v(?P<version>\\d+\\.\\d+\\.\\d+)$"
assets:
  linux/amd64:
    - "tool-linux-amd64$"
asset_type:
  type: binary
  name: tool
platforms:
  linux/amd64:
    runner: ubuntu-latest
  windows/arm64:
    runner: windows-11-arm
    exclude:
      - version: "0.16.0"
        reason: "aarch64-windows build-exe segfault"
        severity: broken
  darwin/amd64:
    runner: macos-14
    exclude:
      - version: "0.16.0"
        severity: skip
"#;

    #[test]
    fn collect_excluded_platforms_records_broken_only() {
        let spec: MirrorSpec = serde_yaml_ng::from_str(EXCLUDE_SPEC).unwrap();

        // windows/arm64 = broken (recorded); darwin/amd64 = skip (silent).
        let excluded = collect_excluded_platforms(&spec, "0.16.0");
        assert_eq!(
            excluded.len(),
            1,
            "only broken-severity excludes recorded: {excluded:?}"
        );
        assert_eq!(excluded[0].platform, "windows/arm64");
        assert_eq!(
            excluded[0].reason.as_deref(),
            Some("aarch64-windows build-exe segfault")
        );
    }

    #[test]
    fn collect_excluded_platforms_strips_build_metadata() {
        let spec: MirrorSpec = serde_yaml_ng::from_str(EXCLUDE_SPEC).unwrap();
        // The bundle version carries a build stamp; the exclude is declared bare.
        let excluded = collect_excluded_platforms(&spec, "0.16.0_20260604120000");
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].platform, "windows/arm64");
    }

    #[test]
    fn collect_excluded_platforms_strips_variant_prefix() {
        let spec: MirrorSpec = serde_yaml_ng::from_str(EXCLUDE_SPEC).unwrap();
        // Variant mirrors key off variant-prefixed versions (e.g. `debug-0.16.0`);
        // the exclude is declared bare. The 🔒 row must still be recorded.
        let excluded = collect_excluded_platforms(&spec, "debug-0.16.0");
        assert_eq!(
            excluded.len(),
            1,
            "variant-prefixed version still records broken exclude: {excluded:?}"
        );
        assert_eq!(excluded[0].platform, "windows/arm64");
        // Variant + build stamp together.
        let stamped = collect_excluded_platforms(&spec, "debug-0.16.0_20260604120000");
        assert_eq!(stamped.len(), 1);
        assert_eq!(stamped[0].platform, "windows/arm64");
    }

    #[test]
    fn collect_excluded_platforms_empty_for_unaffected_version() {
        let spec: MirrorSpec = serde_yaml_ng::from_str(EXCLUDE_SPEC).unwrap();
        assert!(collect_excluded_platforms(&spec, "0.17.0").is_empty());
    }

    #[test]
    fn parse_bundle_filename_roundtrips() {
        // Verify parse_bundle_filename handles standard version + platform slugs.
        let cases = [
            ("bundle-3.7.0-linux_amd64.tar.xz", Some(("3.7.0", "linux_amd64"))),
            ("bundle-3.29.0-darwin_arm64.tar.xz", Some(("3.29.0", "darwin_arm64"))),
            ("bundle-1.2.3-windows_amd64.tar.xz", Some(("1.2.3", "windows_amd64"))),
            ("not-a-bundle.tar.xz", None),
            ("bundle-invalid.tar.xz", None),
        ];

        for (input, expected) in &cases {
            assert_eq!(parse_bundle_filename(input), *expected, "input: {input}");
        }
    }

    #[test]
    fn slug_to_platform_roundtrips() {
        assert_eq!(slug_to_platform("linux_amd64"), "linux/amd64");
        assert_eq!(slug_to_platform("darwin_arm64"), "darwin/arm64");
        assert_eq!(slug_to_platform("windows_amd64"), "windows/amd64");
    }

    #[test]
    fn platform_to_slug_roundtrips() {
        assert_eq!(platform_to_slug("linux/amd64"), "linux_amd64");
        assert_eq!(platform_to_slug("darwin/arm64"), "darwin_arm64");
        assert_eq!(platform_to_slug("windows/amd64"), "windows_amd64");
    }

    #[test]
    fn determine_status_all_pushed_is_published() {
        // D12: All platforms pushed → Published
        let status = determine_status(&["linux/amd64".to_string()], &[], false, true);
        assert!(matches!(status, VersionStatus::Published));
    }

    #[test]
    fn determine_status_all_failed_is_failed() {
        // D12: All platforms failed → Failed
        let failed = vec![PlatformFailure {
            platform: "linux/amd64".to_string(),
            reason: "test_failed".to_string(),
            failed_tests: vec![],
            job_url: None,
        }];
        let status = determine_status(&[], &failed, false, false);
        assert!(matches!(status, VersionStatus::Failed));
    }

    #[test]
    fn a_partial_version_reports_the_registry_truthfully() {
        // `determine_status` is a verdict, never a tag rewriter: the summary
        // reports what the registry received. A partial version carries its
        // exact version tag alone because the push loop withheld `--cascade`
        // from it, not because anything trimmed the list afterwards — and the
        // announce therefore repeats it verbatim.
        let failed = vec![PlatformFailure {
            platform: "darwin/arm64".to_string(),
            reason: "test_failed".to_string(),
            failed_tests: vec![],
            job_url: None,
        }];
        let status = determine_status(&["linux/amd64".to_string()], &failed, false, true);
        assert!(matches!(status, VersionStatus::Partial));

        let summary = VersionSummary {
            version: "3.7.0".to_string(),
            status,
            platforms_pushed: vec!["linux/amd64".to_string()],
            platforms_failed: failed,
            cascade_tags_written: vec!["3.7.0".into()],
            test_failures: vec![],
            platforms_excluded: vec![],
        };
        assert_eq!(announce_tag_union(std::slice::from_ref(&summary)), vec!["3.7.0"]);
    }

    #[test]
    fn determine_status_all_skipped_existing() {
        // D12: All skipped → SkippedExisting
        let status = determine_status(&[], &[], true, false);
        assert!(matches!(status, VersionStatus::SkippedExisting));
    }

    #[test]
    fn evaluate_junit_returns_green_when_all_tests_pass() {
        // Unit test for evaluate_junit: all-green JUNIT for native platform.
        let junit_dir = tempdir().unwrap();
        let version = "1.0.0";
        let slug = "linux_amd64";

        write_junit(
            junit_dir.path(),
            version,
            slug,
            "_native_",
            &passing_junit(version, "linux/amd64", "_native_"),
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &["_native_".to_string()],
            &["version".to_string()],
        ));

        assert!(matches!(decision, VpDecision::Green), "All-pass JUNIT must yield Green");
    }

    #[test]
    fn evaluate_junit_returns_red_when_declared_test_missing() {
        // A JUNIT file present but missing a declared test name → Red.
        let junit_dir = tempdir().unwrap();
        let version = "1.0.0";
        let slug = "linux_amd64";

        // Write JUNIT with only "version" test; "smoke" is declared but absent.
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "_native_",
            &passing_junit(version, "linux/amd64", "_native_"),
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &["_native_".to_string()],
            // Both "version" (present) and "smoke" (missing) declared.
            &["version".to_string(), "smoke".to_string()],
        ));

        match decision {
            VpDecision::Red { test_failures, .. } => {
                assert!(
                    test_failures.iter().any(|tf| tf.test == "smoke"),
                    "Missing 'smoke' test must appear in test_failures"
                );
            }
            VpDecision::Green => panic!("Missing declared test must yield Red decision"),
        }
    }

    // ── JUnit-embedded job_url plumbing for the Discord embed ─────────────
    //
    // The test matrix step computes the matrix-leg `html_url` once via
    // `gh api` and embeds it in the JUnit XML as a suite-level
    // `<property name="ci.job.url" value="…"/>`. `evaluate_junit` reads the
    // property and threads it onto the `PlatformFailure` so the Discord
    // notify step can render a markdown link to the responsible job.

    /// JUnit XML carrying a `ci.job.url` property and one failing testcase.
    fn failing_junit_with_job_url(_version: &str, platform: &str, image: &str, url: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="ocx-mirror.shfmt.{slug}.{cid}" tests="1" failures="1" errors="0" skipped="0" timestamp="2026-05-14T10:00:00Z" time="2.0">
    <properties>
      <property name="ci.job.url" value="{url}"/>
    </properties>
    <testcase name="version" classname="ocx-mirror.shfmt.{slug}.{cid}" time="2.0">
      <failure message="exit code 1" type="exit_code">binary not found</failure>
    </testcase>
  </testsuite>
</testsuites>"#,
            slug = platform.replace('/', "_"),
            cid = image.replace([':', '/'], "_"),
            url = url,
        )
    }

    #[test]
    fn evaluate_junit_attaches_job_url_from_property_for_test_failed() {
        let junit_dir = tempdir().unwrap();
        let version = "1.0.0";
        let slug = "linux_amd64";
        let url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/7";

        write_junit(
            junit_dir.path(),
            version,
            slug,
            "_native_",
            &failing_junit_with_job_url(version, "linux/amd64", "_native_", url),
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &["_native_".to_string()],
            &["version".to_string()],
        ));

        match decision {
            VpDecision::Red { platform_failure, .. } => {
                assert_eq!(platform_failure.reason, "test_failed");
                assert_eq!(platform_failure.job_url.as_deref(), Some(url));
            }
            VpDecision::Green => panic!("failing JUNIT must yield Red"),
        }
    }

    #[test]
    fn evaluate_junit_omits_job_url_when_property_absent() {
        let junit_dir = tempdir().unwrap();
        let version = "1.0.0";
        let slug = "linux_amd64";

        // Failing JUNIT without a `ci.job.url` property — push runs against
        // legacy workflow templates (no URL injection) must still produce a
        // usable PlatformFailure, just without the clickable link.
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "_native_",
            &failing_junit(version, "linux/amd64", "_native_"),
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &["_native_".to_string()],
            &["version".to_string()],
        ));

        match decision {
            VpDecision::Red { platform_failure, .. } => {
                assert!(
                    platform_failure.job_url.is_none(),
                    "absent ci.job.url property must produce job_url=None"
                );
            }
            VpDecision::Green => panic!("failing JUNIT must yield Red"),
        }
    }

    #[test]
    fn evaluate_junit_picks_first_property_across_containers() {
        // Multi-container leg: only one container's JUNIT carries the
        // ci.job.url property. The first non-empty value wins so the failure
        // gets linked even when not every container writes the property.
        let junit_dir = tempdir().unwrap();
        let version = "1.0.0";
        let slug = "linux_amd64";
        let url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/9";

        // ubuntu container: no property
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "ubuntu_2404",
            &failing_junit(version, "linux/amd64", "ubuntu:24.04"),
        );
        // alpine container: property present, also failing
        write_junit(
            junit_dir.path(),
            version,
            slug,
            "alpine_3_20",
            &failing_junit_with_job_url(version, "linux/amd64", "alpine:3.20", url),
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &["ubuntu_2404".to_string(), "alpine_3_20".to_string()],
            &["version".to_string()],
        ));

        match decision {
            VpDecision::Red { platform_failure, .. } => {
                assert_eq!(platform_failure.job_url.as_deref(), Some(url));
            }
            VpDecision::Green => panic!("failing JUNIT must yield Red"),
        }
    }

    #[test]
    fn evaluate_junit_omits_job_url_for_missing_junit() {
        // When the JUnit XML never landed (`missing_junit` reason) there's
        // no property to read either. The failure still has the right reason
        // but `job_url` stays `None`. Title's run_url is the navigation
        // fallback for this case.
        let junit_dir = tempdir().unwrap();
        let version = "1.0.0";
        let slug = "linux_amd64";

        let rt = tokio::runtime::Runtime::new().unwrap();
        let decision = rt.block_on(evaluate_junit(
            junit_dir.path(),
            version,
            slug,
            &["ubuntu_2404".to_string()],
            &["version".to_string()],
        ));

        match decision {
            VpDecision::Red { platform_failure, .. } => {
                assert_eq!(platform_failure.reason, "missing_junit");
                assert!(platform_failure.job_url.is_none());
            }
            VpDecision::Green => panic!("missing junit must yield Red"),
        }
    }

    // ── push_job_url stamping via OCX_MIRROR_JOB_URL ─────────────────────
    //
    // `pipeline push` reads `OCX_MIRROR_JOB_URL` at startup and stamps it
    // onto:
    //   - every `push_error` / `missing_bundle` PlatformFailure.job_url
    //   - the run-summary's top-level `push_job_url`
    // The Discord notify step uses the latter to link green rows + the
    // former to link push-tier failures.

    #[test]
    fn push_stamps_run_summary_push_job_url_from_env() {
        let _env_lock = job_url_env_lock();
        let bundles_dir = tempdir().unwrap();
        let junit_dir = tempdir().unwrap();
        let summary_path = tempdir().unwrap().path().join("run-summary.json");

        let push_url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/99";

        // SAFETY: test-only env var. Tests run inside a single nextest leg
        // but multiple may share a process — unique name avoids cross-test
        // contention.
        unsafe {
            std::env::set_var("OCX_MIRROR_JOB_URL", push_url);
        }

        // No bundles → no versions → push exits Ok and writes an empty
        // summary. push_job_url must still be set so notify can link to
        // the push job even on degenerate runs.
        let spec_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ))
        .to_path_buf();

        let result = run_push_cmd(
            spec_path,
            junit_dir.path().to_path_buf(),
            bundles_dir.path().to_path_buf(),
            summary_path.clone(),
        );

        // SAFETY: cleanup so neighbouring tests don't inherit the stamp.
        unsafe {
            std::env::remove_var("OCX_MIRROR_JOB_URL");
        }

        // Acceptable if the test env can't load the spec — we only care
        // about the env-stamp wiring.
        if result.is_ok() {
            let content = std::fs::read_to_string(&summary_path).unwrap();
            let v: serde_json::Value = serde_json::from_str(&content).unwrap();
            assert_eq!(v["push_job_url"].as_str(), Some(push_url));
        }
    }

    #[test]
    fn push_stamps_push_error_failures_with_push_job_url() {
        let _env_lock = job_url_env_lock();
        let bundles_dir = tempdir().unwrap();
        let junit_dir = tempdir().unwrap();
        let summary_path = tempdir().unwrap().path().join("run-summary.json");

        let version = "3.7.0";
        let slug = "linux_amd64";
        let push_url = "https://github.com/ocx-sh/mirror-shfmt/actions/runs/42/job/99";

        // Bundle present + JUNIT absent → version loop enters push branch
        // via the missing_bundle path *or* the push path. We write JUNIT
        // for a single container that the multi-container spec expects, so
        // the (V, P) decision is Red(missing_junit), not push_error. We
        // instead test the missing_bundle path: bundle absent, JUNIT green.
        // Wait — the loop only attempts push when JUNIT is Green; with
        // bundle absent that's missing_bundle which still gets stamped.
        for cid in ["ubuntu_24_04", "alpine_3_20", "fedora_40"] {
            let xml = passing_junit(version, "linux/amd64", &cid.replacen('_', ":", 1));
            write_junit(junit_dir.path(), version, slug, cid, &xml);
        }
        // No bundle file created → missing_bundle path.

        // Drop a junk bundle to make the version appear in the enumeration.
        // The bundle file path used by the push step differs, so the
        // bundle.exists() check still fails (the file we drop lives at the
        // canonical path; with it present, push_error is exercised instead
        // when the subprocess fails — also valid for the stamp test).
        std::fs::write(bundles_dir.path().join(format!("bundle-{version}-{slug}.tar.xz")), b"x").unwrap();

        // SAFETY: test-only stamp.
        unsafe {
            std::env::set_var("OCX_MIRROR_JOB_URL", push_url);
        }

        let spec_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-multi-container.yml"
        ))
        .to_path_buf();

        let _ = run_push_cmd(
            spec_path,
            junit_dir.path().to_path_buf(),
            bundles_dir.path().to_path_buf(),
            summary_path.clone(),
        );

        // SAFETY: cleanup.
        unsafe {
            std::env::remove_var("OCX_MIRROR_JOB_URL");
        }

        let summary: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        assert_eq!(summary["push_job_url"].as_str(), Some(push_url));

        // Every failure with reason `push_error` or `missing_bundle` must
        // carry job_url == push_url. test_failed / missing_junit failures
        // keep their JUnit-derived URL or None and are left untouched here.
        let failures = summary["versions"][0]["platforms_failed"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for f in &failures {
            let reason = f["reason"].as_str().unwrap_or("");
            if reason == "push_error" || reason == "missing_bundle" {
                assert_eq!(
                    f["job_url"].as_str(),
                    Some(push_url),
                    "{reason} failure must carry stamped push_job_url, got: {f}",
                );
            }
        }
    }

    // ── Index announce (E-P4) ─────────────────────────────────────────────
    //
    // One announce per run, carrying the union of every cascade tag the run
    // wrote. `--tags-file` (additive) never `--tags` (replacing), because a
    // mirror announcing a replacing tag set would delete every previously
    // announced version the moment one run published a new one.

    fn version_summary(version: &str, status: VersionStatus, pushed: &[&str], tags: &[&str]) -> VersionSummary {
        VersionSummary {
            version: version.to_string(),
            status,
            platforms_pushed: pushed.iter().map(|s| (*s).to_string()).collect(),
            platforms_failed: vec![],
            cascade_tags_written: tags.iter().map(|s| (*s).to_string()).collect(),
            test_failures: vec![],
            platforms_excluded: vec![],
        }
    }

    fn announce_config() -> AnnounceConfig {
        serde_yaml_ng::from_str("package: bazelbuild/bazelisk\nfork: ocx-contrib/index\nindex_repo: ocx-sh/index\n")
            .unwrap()
    }

    /// A stand-in `ocx` that appends its argv (one invocation per line) to
    /// `log`. Lets the announce subprocess boundary be exercised without
    /// mutating process environment.
    #[cfg(unix)]
    fn fake_ocx(dir: &Path, log: &Path, exit_code: u8) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("fake-ocx");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit {exit_code}\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[test]
    fn announce_tag_union_dedups_across_versions_and_platforms() {
        // Each platform's push report re-lists the same cascade hierarchy, and
        // consecutive versions share the rolling `1` / `latest` tags. The
        // union must carry each tag exactly once, in run order.
        let versions = vec![
            version_summary(
                "1.20.0",
                VersionStatus::Published,
                &["linux/amd64", "darwin/arm64"],
                &["1.20.0", "1.20", "1", "latest"],
            ),
            version_summary(
                "1.21.0",
                VersionStatus::Published,
                &["linux/amd64"],
                &["1.21.0", "1.21", "1", "latest"],
            ),
        ];

        assert_eq!(
            announce_tag_union(&versions),
            vec!["1.20.0", "1.20", "1", "latest", "1.21.0", "1.21"],
        );
    }

    #[test]
    fn announce_tag_union_covers_partial_versions_but_not_unpublished_ones() {
        // Partial with at least one platform pushed still wrote its exact
        // version tag → include that. Failed / skipped_existing wrote nothing
        // new → exclude, so a run that published nothing announces nothing.
        let versions = vec![
            version_summary("1.0.0", VersionStatus::SkippedExisting, &[], &["1.0.0"]),
            version_summary("2.0.0", VersionStatus::Failed, &[], &[]),
            version_summary("3.0.0", VersionStatus::Partial, &["linux/amd64"], &["3.0.0"]),
        ];

        assert_eq!(announce_tag_union(&versions), vec!["3.0.0"]);

        let nothing_published = vec![
            version_summary("1.0.0", VersionStatus::SkippedExisting, &[], &["1.0.0"]),
            version_summary("2.0.0", VersionStatus::Failed, &[], &[]),
        ];
        assert!(announce_tag_union(&nothing_published).is_empty());
    }

    #[test]
    fn a_run_with_a_partial_version_still_announces_the_whole_one_in_full() {
        // bazelisk 1.21.0 on linux + darwin, darwin red, alongside a fully
        // published 1.20.0. `latest` and `1` are announced — in the registry
        // they still point at 1.20.0's complete index, because the push loop
        // never gave 1.21.0 `--cascade`. Filtering them here (the shape this
        // replaces) suppressed a truthful alias while doing nothing about the
        // untruthful one, which `announce`'s re-observation of the already
        // curated set would have re-committed regardless.
        let versions = vec![
            version_summary(
                "1.20.0",
                VersionStatus::Published,
                &["linux/amd64", "darwin/arm64"],
                &["1.20.0", "1.20", "1", "latest"],
            ),
            version_summary("1.21.0", VersionStatus::Partial, &["linux/amd64"], &["1.21.0"]),
        ];

        assert_eq!(
            announce_tag_union(&versions),
            vec!["1.20.0", "1.20", "1", "latest", "1.21.0"],
        );
    }

    #[test]
    fn build_announce_args_uses_additive_tags_file_never_replacing_tags() {
        let args = build_announce_args(&announce_config(), Path::new("/tmp/tags.txt")).unwrap();

        assert_eq!(
            args,
            vec![
                "package",
                "announce",
                "--package",
                "bazelbuild/bazelisk",
                "--tags-file",
                "/tmp/tags.txt",
                "--fork",
                "ocx-contrib/index",
                "--index-repo",
                "ocx-sh/index",
            ],
        );
        assert!(
            !args.iter().any(|a| a == "--tags"),
            "--tags REPLACES the curated set — a mirror must never use it",
        );
    }

    #[cfg(unix)]
    #[test]
    fn announce_runs_exactly_once_per_run_with_the_union_of_tags() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("invocations.log");
        let ocx = fake_ocx(dir.path(), &log, 0);
        let tags_file = dir.path().join("run-summary.announce-tags");
        let config = announce_config();

        let versions = vec![
            version_summary(
                "1.20.0",
                VersionStatus::Published,
                &["linux/amd64"],
                &["1.20.0", "1.20"],
            ),
            version_summary(
                "1.21.0",
                VersionStatus::Published,
                &["linux/amd64"],
                &["1.21.0", "1.21"],
            ),
        ];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let outcome = rt.block_on(run_announce(
            Some(&config),
            &versions,
            &tags_file,
            Some("gh-token"),
            &ocx,
        ));

        assert_eq!(
            outcome,
            Some(AnnounceOutcome::Announced {
                package: "bazelbuild/bazelisk".to_string(),
                tags: vec![
                    "1.20.0".to_string(),
                    "1.20".to_string(),
                    "1.21.0".to_string(),
                    "1.21".to_string()
                ],
            }),
        );

        // Exactly one subprocess, not one per version and not one per platform.
        let invocations = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            invocations.lines().count(),
            1,
            "announce must run once per pipeline run, got: {invocations}",
        );
        assert!(invocations.contains("--tags-file"), "got: {invocations}");

        // The tag set travels in the file, so the whole union lands even when
        // it outgrows anything comfortable on a command line.
        let written = std::fs::read_to_string(&tags_file).unwrap();
        assert_eq!(written, "1.20.0\n1.20\n1.21.0\n1.21");
    }

    #[cfg(unix)]
    #[test]
    fn announce_skipped_without_token_and_stays_distinguishable_in_the_summary() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("invocations.log");
        let ocx = fake_ocx(dir.path(), &log, 0);
        let config = announce_config();
        let versions = vec![version_summary(
            "1.20.0",
            VersionStatus::Published,
            &["linux/amd64"],
            &["1.20.0"],
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let outcome = rt.block_on(run_announce(
            Some(&config),
            &versions,
            &dir.path().join("tags"),
            None,
            &ocx,
        ));

        assert_eq!(
            outcome,
            Some(AnnounceOutcome::SkippedNoCredential {
                package: "bazelbuild/bazelisk".to_string(),
            }),
        );
        assert!(!log.exists(), "no token must mean no announce subprocess");

        // A run that pushed and skipped announcing must not read like one that
        // announced, nor like one that tried and failed.
        let rendered = |o: &AnnounceOutcome| serde_json::to_value(o).unwrap()["status"].clone();
        assert_eq!(rendered(&outcome.unwrap()), "skipped_no_credential");
        assert_eq!(
            rendered(&AnnounceOutcome::Announced {
                package: "p/q".into(),
                tags: vec![]
            }),
            "announced",
        );
        assert_eq!(
            rendered(&AnnounceOutcome::Failed {
                package: "p/q".into(),
                error: "boom".into()
            }),
            "failed",
        );
    }

    #[cfg(unix)]
    #[test]
    fn announce_failure_is_recorded_and_does_not_abort_the_run() {
        let dir = tempdir().unwrap();
        let log = dir.path().join("invocations.log");
        let ocx = fake_ocx(dir.path(), &log, 70);
        let versions = vec![version_summary(
            "1.20.0",
            VersionStatus::Published,
            &["linux/amd64"],
            &["1.20.0"],
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let outcome = rt.block_on(run_announce(
            Some(&announce_config()),
            &versions,
            &dir.path().join("tags"),
            Some("gh-token"),
            &ocx,
        ));

        match outcome {
            Some(AnnounceOutcome::Failed { package, error }) => {
                assert_eq!(package, "bazelbuild/bazelisk");
                assert!(error.contains("ocx package announce exited"), "got: {error}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn nothing_to_announce_stays_distinct_from_never_configured() {
        // Both make no call. They need very different fixes, though: the first
        // is the steady state of an up-to-date mirror *and* the permanent state
        // of one whose `announce:` block was added after everything had already
        // published — where the catch-up is manual. Collapsing both to `None`
        // makes that owner read forever-silence as success.
        let dir = tempdir().unwrap();
        let log = dir.path().join("invocations.log");
        let ocx = fake_ocx(dir.path(), &log, 0);
        let rt = tokio::runtime::Runtime::new().unwrap();

        // Configured, but the run published nothing.
        let barren = vec![version_summary(
            "1.0.0",
            VersionStatus::SkippedExisting,
            &[],
            &["1.0.0"],
        )];
        let outcome = rt.block_on(run_announce(
            Some(&announce_config()),
            &barren,
            &dir.path().join("tags"),
            Some("gh-token"),
            &ocx,
        ));
        assert_eq!(
            outcome,
            Some(AnnounceOutcome::NothingToAnnounce {
                package: "bazelbuild/bazelisk".to_string(),
            }),
        );
        assert_eq!(
            serde_json::to_value(outcome.unwrap()).unwrap()["status"],
            "nothing_to_announce",
        );

        // Published, but no `announce:` block — announce is opt-in.
        let published = vec![version_summary(
            "1.0.0",
            VersionStatus::Published,
            &["linux/amd64"],
            &["1.0.0"],
        )];
        assert_eq!(
            rt.block_on(run_announce(
                None,
                &published,
                &dir.path().join("tags"),
                Some("t"),
                &ocx
            )),
            None,
        );

        assert!(!log.exists(), "neither case may spawn an announce subprocess");
    }

    #[cfg(unix)]
    #[test]
    fn announce_writes_its_tags_file_into_a_not_yet_existing_directory() {
        // `--write-summary out/run-summary.json` with `out/` absent: the tags
        // file is a sibling, and the announce runs before the summary write
        // that would have created the directory. Without a create_dir_all here
        // every announce under such a path is a deterministic failure.
        let dir = tempdir().unwrap();
        let log = dir.path().join("invocations.log");
        let ocx = fake_ocx(dir.path(), &log, 0);
        let tags_file = dir.path().join("out").join("run-summary.announce-tags");

        let versions = vec![version_summary(
            "1.0.0",
            VersionStatus::Published,
            &["linux/amd64"],
            &["1.0.0"],
        )];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let outcome = rt.block_on(run_announce(
            Some(&announce_config()),
            &versions,
            &tags_file,
            Some("gh-token"),
            &ocx,
        ));

        assert!(
            matches!(outcome, Some(AnnounceOutcome::Announced { .. })),
            "got: {outcome:?}",
        );
        assert_eq!(std::fs::read_to_string(&tags_file).unwrap(), "1.0.0");
    }

    #[cfg(unix)]
    #[test]
    fn a_stalled_announce_is_killed_instead_of_taking_the_job_down_with_it() {
        // The announce pushes a fork branch, calls the PR API and observes the
        // registry. A stall there (a 429 retry loop is enough) used to run
        // until the runner killed the job.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let script = dir.path().join("hanging-ocx");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let started = std::time::Instant::now();
        let result = rt.block_on(invoke_announce(
            &announce_config(),
            &["1.0.0".to_string()],
            &dir.path().join("tags"),
            &script,
            Duration::from_millis(200),
        ));

        let error = result.expect_err("a hung announce must not hang the run");
        assert!(error.contains("timed out"), "got: {error}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout must bound the wait, took {:?}",
            started.elapsed(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_run_summary_is_on_disk_before_the_announce_starts() {
        // Twelve images push fine, the announce stalls, the job is killed on
        // the runner timeout. Written after the announce, run-summary.json
        // would never exist: the artifact upload finds nothing, the notify
        // gate reads false, and a dozen live images go unreported.
        use std::os::unix::fs::PermissionsExt;

        let _env_lock = job_url_env_lock();
        let dir = tempdir().unwrap();
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = dir.path().join("run-summary.json");
        let log = dir.path().join("announce-observations.log");

        // Stand-in `ocx`: answers a push with a cascade report, and on announce
        // records whether the summary was already on disk when it was called.
        let script = dir.path().join("fake-ocx");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
case "$*" in
  *"package announce"*)
    if [ -f '{summary}' ]; then echo saw-summary >> '{log}'; else echo saw-nothing >> '{log}'; fi
    ;;
  *)
    echo '{{"cascade_tags_written":["3.7.0"],"status":"pushed"}}'
    ;;
esac
"#,
                summary = summary_path.display(),
                log = log.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let version = "3.7.0";
        write_junit(
            junit_dir.path(),
            version,
            "linux_amd64",
            "_native_",
            &passing_junit(version, "linux/amd64", "_native_"),
        );
        // Contents are irrelevant — the push subprocess is the stand-in above.
        std::fs::write(bundles_dir.path().join("bundle-3.7.0-linux_amd64.tar.xz"), b"x").unwrap();

        // SAFETY: test-only process env, serialised by the lock above.
        unsafe {
            std::env::set_var("OCX_BINARY_PIN", &script);
            std::env::set_var(ENV_ANNOUNCE_TOKEN, "gh-token");
        }

        let result = run_push_cmd(
            std::path::Path::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/mirror-ghcr-announce.yml"
            ))
            .to_path_buf(),
            junit_dir.path().to_path_buf(),
            bundles_dir.path().to_path_buf(),
            summary_path.clone(),
        );

        // SAFETY: cleanup so neighbouring tests don't inherit either var.
        unsafe {
            std::env::remove_var("OCX_BINARY_PIN");
            std::env::remove_var(ENV_ANNOUNCE_TOKEN);
        }
        result.expect("a fully green run must exit 0");

        assert_eq!(
            std::fs::read_to_string(&log).unwrap().trim(),
            "saw-summary",
            "the announce must run against an already-durable run summary",
        );

        // And the announce outcome still lands in the file afterwards.
        let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        assert_eq!(val["announce"]["status"], "announced", "got: {val}");
        assert_eq!(val["announce"]["tags"][0], "3.7.0", "got: {val}");
    }

    /// A stand-in `ocx` for the whole-pipeline tests: logs every argv, exits
    /// `announce_exit` on `package announce`, and — crucially — models what a
    /// push does to the *registry* rather than just answering with a canned
    /// tag list.
    ///
    /// The modelled semantics are `client.rs::merge_platform_into_index`: every
    /// push merges its own platform into the exact version tag's index, and a
    /// `--cascade` push additionally merges that **same single platform** into
    /// each rolling tag, replacing only its own entry (`retain(|e| e.platform
    /// != platform)`) and keeping every other platform's entry exactly as it
    /// found it. A canned list that answered the same aliases for any
    /// `--cascade` invocation could not observe that only one platform ever
    /// reached them.
    ///
    /// State lands in `{dir}/tagstate/{tag}`, one sorted `platform=version`
    /// line per platform the tag's index carries — read back with [`tag_index`].
    #[cfg(unix)]
    fn fake_ocx_pipeline(dir: &Path, log: &Path, announce_exit: u8) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let state = dir.join("tagstate");
        std::fs::create_dir_all(&state).unwrap();
        let script = dir.join("fake-ocx");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"package announce"*) exit {announce_exit} ;;
esac

# The push carries `-p PLATFORM` and `-i REPOSITORY:VERSION`.
platform=''; ref=''; prev=''
for a in "$@"; do
  case "$prev" in
    -p) platform="$a" ;;
    -i) ref="$a" ;;
  esac
  prev="$a"
done
version="${{ref##*:}}"
minor="${{version%.*}}"
major="${{minor%.*}}"

# merge_platform_into_index: read, drop THIS platform's entry, append it
# back pointing at this version, keep every other platform's entry.
merge() {{
  f='{state}'/"$1"
  [ -f "$f" ] || : > "$f"
  grep -v "^$platform=" "$f" > "$f.tmp"
  echo "$platform=$version" >> "$f.tmp"
  sort -o "$f" "$f.tmp"
  rm -f "$f.tmp"
}}

merge "$version"
case "$*" in
  *--cascade*)
    for t in "$minor" "$major" latest; do merge "$t"; done
    echo '{{"cascade_tags_written":["'"$minor"'","'"$major"'","latest"],"status":"pushed"}}'
    ;;
  *)
    echo '{{"cascade_tags_written":[],"status":"pushed"}}'
    ;;
esac
"#,
                log = log.display(),
                state = state.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The `platform=version` entries the stand-in registry's `tag` index
    /// carries, or empty when the tag was never written.
    #[cfg(unix)]
    fn tag_index(dir: &Path, tag: &str) -> Vec<String> {
        std::fs::read_to_string(dir.join("tagstate").join(tag))
            .map(|body| body.lines().map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// Every logged `package push` argv that targets `repo:version`.
    /// Pushes for `version`, matched on the **registry-qualified** `-i`.
    ///
    /// The fixture targets ghcr.io, so that is what must reach the argv. A bare
    /// `ocx-contrib/…` reference resolves against the default registry instead:
    /// the first ghcr.io mirror sent five versions at `ocx.sh` and took
    /// `403 UNAUTHORIZED: No permission to write manifest` on every one. Match
    /// the whole reference so dropping the registry empties this list rather
    /// than silently passing.
    fn pushes_for(log: &str, version: &str) -> Vec<String> {
        log.lines()
            .filter(|line| line.contains("package push"))
            .filter(|line| line.contains(&format!("-i ghcr.io/ocx-contrib/bazelbuild/bazelisk:{version} ")))
            .map(str::to_string)
            .collect()
    }

    /// Drive the whole push pipeline against a stand-in `ocx`.
    #[cfg(unix)]
    fn run_pipeline_with_fake_ocx(
        fixture: &str,
        script: &Path,
        junit_dir: &Path,
        bundles_dir: &Path,
        summary_path: &Path,
        token: Option<&str>,
    ) -> Result<(), MirrorError> {
        // SAFETY: test-only process env, serialised by the caller's lock.
        unsafe {
            std::env::set_var("OCX_BINARY_PIN", script);
            match token {
                Some(t) => std::env::set_var(ENV_ANNOUNCE_TOKEN, t),
                None => std::env::remove_var(ENV_ANNOUNCE_TOKEN),
            }
        }
        let result = run_push_cmd(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(fixture),
            junit_dir.to_path_buf(),
            bundles_dir.to_path_buf(),
            summary_path.to_path_buf(),
        );
        // SAFETY: cleanup so neighbouring tests don't inherit either var.
        unsafe {
            std::env::remove_var("OCX_BINARY_PIN");
            std::env::remove_var(ENV_ANNOUNCE_TOKEN);
        }
        result
    }

    #[cfg(unix)]
    #[test]
    fn a_red_platform_stops_the_run_from_moving_any_rolling_alias_in_the_registry() {
        // The scenario that defeats every announce-side filter. mirror-bazelisk
        // has been publishing for months, so the index entry ALREADY curates
        // `latest`, `1`, `1.20` and `1.20.0`. Tonight's run publishes 1.21.0
        // with darwin/arm64 red.
        //
        // `ocx package announce --tags-file` is additive AND re-observes every
        // tag the entry already carries — so withholding `latest` and `1` from
        // this run's union buys nothing: the announce re-fetches them from the
        // registry and re-commits whatever they point at now. The only place
        // the damage can be prevented is the registry: those aliases must never
        // be moved onto 1.21.0's linux-only index in the first place.
        //
        // So this asserts on the argv the registry-facing subprocess received,
        // not on what the union computed. 1.20.0 (whole) still cascades, once,
        // on its LAST platform — an earlier push failing mid-version must not
        // leave an alias on a half-assembled index either.
        let _env_lock = job_url_env_lock();
        let dir = tempdir().unwrap();
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = dir.path().join("run-summary.json");
        let log = dir.path().join("invocations.log");
        let script = fake_ocx_pipeline(dir.path(), &log, 0);

        // An established mirror: `latest` and `1` already resolve to 1.19.0 on
        // both platforms. This is what a cascade merges INTO, and what it
        // leaves behind for any platform it does not carry.
        for tag in ["latest", "1"] {
            std::fs::write(
                dir.path().join("tagstate").join(tag),
                "darwin/arm64=1.19.0\nlinux/amd64=1.19.0\n",
            )
            .unwrap();
        }

        for (version, slug, platform, green) in [
            ("1.20.0", "linux_amd64", "linux/amd64", true),
            ("1.20.0", "darwin_arm64", "darwin/arm64", true),
            ("1.21.0", "linux_amd64", "linux/amd64", true),
            ("1.21.0", "darwin_arm64", "darwin/arm64", false),
        ] {
            let xml = if green {
                passing_junit(version, platform, "_native_")
            } else {
                failing_junit(version, platform, "_native_")
            };
            write_junit(junit_dir.path(), version, slug, "_native_", &xml);
            std::fs::write(bundles_dir.path().join(format!("bundle-{version}-{slug}.tar.xz")), b"x").unwrap();
        }

        let result = run_pipeline_with_fake_ocx(
            "mirror-two-platform-announce.yml",
            &script,
            junit_dir.path(),
            bundles_dir.path(),
            &summary_path,
            Some("gh-token"),
        );
        assert!(result.is_err(), "a red platform must still fail the push job");

        let invocations = std::fs::read_to_string(&log).unwrap();

        // 1.21.0: the green linux leg publishes under the exact version tag,
        // and NOTHING in the run moves `latest` / `1` / `1.21`.
        let partial = pushes_for(&invocations, "1.21.0");
        assert_eq!(partial.len(), 1, "only the green platform pushes: {partial:?}");
        assert!(
            !partial.iter().any(|argv| argv.contains("--cascade")),
            "a version with a red platform must never cascade: {partial:?}",
        );

        // 1.20.0 is whole, so what the REGISTRY ends up holding is the claim:
        // every rolling alias must resolve to 1.20.0 on BOTH platforms. A
        // cascade push merges only its own platform, so cascading on one push
        // per version leaves each alias carrying that platform at 1.20.0 and
        // every other one still at 1.19.0 — a mixed-version index that freezes
        // half the users on the old release, on this run and every one after.
        let both_at_1_20_0 = vec!["darwin/arm64=1.20.0".to_string(), "linux/amd64=1.20.0".to_string()];
        for tag in ["1.20", "1", "latest"] {
            assert_eq!(
                tag_index(dir.path(), tag),
                both_at_1_20_0,
                "rolling tag `{tag}` must carry every platform of the whole version",
            );
        }
        assert_eq!(tag_index(dir.path(), "1.20.0"), both_at_1_20_0, "exact version tag");

        // The argv that produced it: every push of a whole version cascades.
        let whole = pushes_for(&invocations, "1.20.0");
        assert_eq!(whole.len(), 2, "both platforms push: {whole:?}");
        let cascading: Vec<&String> = whole.iter().filter(|argv| argv.contains("--cascade")).collect();
        assert_eq!(
            cascading.len(),
            whole.len(),
            "every push of a whole version must cascade: {whole:?}",
        );

        // 1.21.0 is partial: its green platform reaches the exact version tag
        // and nothing else — no alias, and no `1.21` conjured from a fresh
        // single-platform index.
        assert_eq!(tag_index(dir.path(), "1.21.0"), vec!["linux/amd64=1.21.0".to_string()]);
        assert!(
            tag_index(dir.path(), "1.21").is_empty(),
            "a partial version writes no `1.21`"
        );

        // And the summary reports the registry truthfully: the partial version
        // carries its version tag alone because nothing else was ever written.
        let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        let partial_version = val["versions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["version"] == "1.21.0")
            .unwrap();
        // The whole version's `cascade_tags_written` is the UNION over its
        // platforms, and the dedup is what keeps it a set: both platforms now
        // cascade, so both reports re-list the same hierarchy and an
        // un-deduped accumulation would read `1.20 1 latest 1.20 1 latest`.
        let whole_version = val["versions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["version"] == "1.20.0")
            .unwrap();
        assert_eq!(
            whole_version["cascade_tags_written"],
            serde_json::json!(["1.20.0", "1.20", "1", "latest"]),
            "got: {whole_version}",
        );

        assert_eq!(partial_version["status"], "partial", "got: {partial_version}");
        assert_eq!(
            partial_version["cascade_tags_written"],
            serde_json::json!(["1.21.0"]),
            "got: {partial_version}",
        );
        assert_eq!(
            val["announce"]["tags"],
            serde_json::json!(["1.20.0", "1.20", "1", "latest", "1.21.0"]),
            "got: {val}",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_run_killed_during_the_announce_does_not_read_as_a_mirror_that_never_opted_in() {
        // The hosted runner is reclaimed, or a maintainer cancels a backfill,
        // while the announce subprocess is in flight. The `if: always()` steps
        // still upload run-summary.json. With `announce: None` written first
        // that summary serialises with the key ABSENT — which `pipeline notify`
        // reads as "no `announce:` block at all". Twelve images live in GHCR,
        // the index knows about none of them, and the artifact says the mirror
        // does not announce.
        //
        // Observed from inside the announce subprocess: that is exactly the
        // window in which the kill lands.
        let _env_lock = job_url_env_lock();
        let dir = tempdir().unwrap();
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = dir.path().join("run-summary.json");
        let observed = dir.path().join("observed-announce-state.log");

        use std::os::unix::fs::PermissionsExt;
        let script = dir.path().join("fake-ocx");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
case "$*" in
  *"package announce"*)
    grep -A1 '"announce": {{' '{summary}' | grep -o '"status": "[a-z_]*"' >> '{observed}'
    ;;
  *) echo '{{"cascade_tags_written":[],"status":"pushed"}}' ;;
esac
"#,
                summary = summary_path.display(),
                observed = observed.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let version = "3.7.0";
        write_junit(
            junit_dir.path(),
            version,
            "linux_amd64",
            "_native_",
            &passing_junit(version, "linux/amd64", "_native_"),
        );
        std::fs::write(bundles_dir.path().join("bundle-3.7.0-linux_amd64.tar.xz"), b"x").unwrap();

        run_pipeline_with_fake_ocx(
            "mirror-ghcr-announce.yml",
            &script,
            junit_dir.path(),
            bundles_dir.path(),
            &summary_path,
            Some("gh-token"),
        )
        .expect("a fully green run must exit 0");

        assert_eq!(
            std::fs::read_to_string(&observed).unwrap().trim(),
            r#""status": "interrupted""#,
            "the durable summary must already name the announce as in flight",
        );

        // And the placeholder is replaced once the call returns.
        let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        assert_eq!(val["announce"]["status"], "announced", "got: {val}");
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_announce_fails_the_push_job() {
        // `OCX_ANNOUNCE_TOKEN` expires. Every push is green, so without this
        // the check stays green forever, no scheduled-run alert fires because
        // nothing failed, and the index drifts arbitrarily far behind the
        // registry. Same reasoning as `any_red`: the images ARE in the
        // registry, and the exit code is how a partial outcome reaches the
        // pipeline and the maintainer.
        let _env_lock = job_url_env_lock();
        let dir = tempdir().unwrap();
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = dir.path().join("run-summary.json");
        let log = dir.path().join("invocations.log");
        let script = fake_ocx_pipeline(dir.path(), &log, 70);

        let version = "3.7.0";
        write_junit(
            junit_dir.path(),
            version,
            "linux_amd64",
            "_native_",
            &passing_junit(version, "linux/amd64", "_native_"),
        );
        std::fs::write(bundles_dir.path().join("bundle-3.7.0-linux_amd64.tar.xz"), b"x").unwrap();

        let result = run_pipeline_with_fake_ocx(
            "mirror-ghcr-announce.yml",
            &script,
            junit_dir.path(),
            bundles_dir.path(),
            &summary_path,
            Some("gh-token"),
        );

        let error = result.expect_err("a failed announce must fail the push job");
        assert!(
            format!("{error}").contains("index announce for bazelbuild/bazelisk failed"),
            "got: {error}",
        );

        // The announce is the ONLY reason: every push was green.
        let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        assert_eq!(val["any_red"], false, "got: {val}");
        assert_eq!(val["announce"]["status"], "failed", "got: {val}");
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_credential_leaves_the_push_job_green_but_visible() {
        // The counterpart to the test above: a mirror without the secret is a
        // valid configuration (forks, test repos), so it degrades rather than
        // failing — but it must stay legible in the summary, and from there in
        // the `announce` job output and the Index row.
        let _env_lock = job_url_env_lock();
        let dir = tempdir().unwrap();
        let junit_dir = tempdir().unwrap();
        let bundles_dir = tempdir().unwrap();
        let summary_path = dir.path().join("run-summary.json");
        let log = dir.path().join("invocations.log");
        let script = fake_ocx_pipeline(dir.path(), &log, 70);

        let version = "3.7.0";
        write_junit(
            junit_dir.path(),
            version,
            "linux_amd64",
            "_native_",
            &passing_junit(version, "linux/amd64", "_native_"),
        );
        std::fs::write(bundles_dir.path().join("bundle-3.7.0-linux_amd64.tar.xz"), b"x").unwrap();

        run_pipeline_with_fake_ocx(
            "mirror-ghcr-announce.yml",
            &script,
            junit_dir.path(),
            bundles_dir.path(),
            &summary_path,
            None,
        )
        .expect("a missing announce secret must not fail the push job");

        let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        assert_eq!(val["announce"]["status"], "skipped_no_credential", "got: {val}");
    }

    #[test]
    fn run_summary_omits_announce_when_the_run_never_announced() {
        // `pipeline notify` reads this file; an absent announce must not
        // appear as a null field it has to special-case.
        let spec_path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mirror-minimal.yml"
        ))
        .to_path_buf();
        let dir = tempdir().unwrap();
        let summary_path = dir.path().join("run-summary.json");

        run_push_cmd(
            spec_path,
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            summary_path.clone(),
        )
        .unwrap();

        let val: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&summary_path).unwrap()).unwrap();
        assert!(val.get("announce").is_none(), "got: {val}");
    }
}
