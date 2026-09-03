// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! Unit tests for `sign:` resolution and the `ocx package sign` argv.
//!
//! Everything here is offline: `resolve_sign` takes its environment and
//! filesystem as closures (C-054), so the unset-variable, unreadable-file and
//! oversized-file paths are reachable without touching either — and without
//! `env::set_var`, which is a data race across the test binary's threads.

use std::collections::BTreeMap;
use std::path::PathBuf;

use ocx_lib::cli::ExitCode;

use super::*;
use crate::spec::{KeyFullConfig, SignConfig};

/// An environment with nothing in it.
fn no_env(_: &str) -> Option<OsString> {
    None
}

/// A filesystem where every read fails.
fn no_files(path: &Path) -> io::Result<Vec<u8>> {
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no fixture for {}", path.display()),
    ))
}

fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
    let map: BTreeMap<String, OsString> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), OsString::from(*v)))
        .collect();
    move |name: &str| map.get(name).cloned()
}

fn files_of(pairs: &[(&'static str, &'static [u8])]) -> impl Fn(&Path) -> io::Result<Vec<u8>> + use<> {
    let map: BTreeMap<PathBuf, Vec<u8>> = pairs.iter().map(|(k, v)| (PathBuf::from(*k), (*v).to_vec())).collect();
    move |path: &Path| {
        map.get(path)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "absent"))
    }
}

fn keyless(fulcio: Option<Ref>, rekor: Option<Ref>, identity_token: Option<Ref>) -> SignConfig {
    SignConfig {
        keyless: Some(KeylessConfig {
            fulcio,
            rekor,
            identity_token,
        }),
        key: None,
    }
}

fn key_full(reference: Ref, passphrase: Option<Ref>, rekor: Option<Ref>) -> SignConfig {
    SignConfig {
        keyless: None,
        key: Some(KeyConfig::Full(KeyFullConfig {
            reference,
            passphrase,
            rekor,
        })),
    }
}

// ── C-052: the four argv shapes ─────────────────────────────────────────────

/// Keyless with nothing named still emits BOTH endpoints, from the
/// mirror-owned constants.
///
/// This is the whole point of D1's amendment: leaving them off would let a
/// per-machine `[trust.sigstore]` decide what this mirror publishes against,
/// and that table is one global shared with the *consuming* side.
#[test]
fn keyless_defaults_name_both_endpoints_explicitly() {
    let resolved = resolve_sign(&keyless(None, None, None), &no_env, &no_files).expect("no refs to resolve");

    assert_eq!(
        sign_push_args(Some(&resolved)),
        [
            "--sign",
            "--fulcio-url",
            DEFAULT_FULCIO_URL,
            "--rekor-url",
            DEFAULT_REKOR_URL,
        ],
    );
}

/// Named endpoints replace the defaults, in the same fixed order — and the
/// identity token is nowhere in the argv (C-054).
#[test]
fn keyless_named_endpoints_travel_as_flags_and_the_token_does_not() {
    let lookup = env_of(&[
        ("SIGSTORE_FULCIO_URL", "http://localhost:5555"),
        ("SIGSTORE_REKOR_URL", "http://localhost:3000"),
        ("CI_TOKEN", "eyJhbGciOi.THE-SECRET.sig"),
    ]);
    let config = keyless(
        Some(Ref::Env("SIGSTORE_FULCIO_URL".into())),
        Some(Ref::Env("SIGSTORE_REKOR_URL".into())),
        Some(Ref::Env("CI_TOKEN".into())),
    );

    let resolved = resolve_sign(&config, &lookup, &no_files).expect("every ref resolves");
    let args = sign_push_args(Some(&resolved));

    assert_eq!(
        args,
        [
            "--sign",
            "--fulcio-url",
            "http://localhost:5555",
            "--rekor-url",
            "http://localhost:3000",
        ],
    );
    // The load-bearing half: a token on argv is world-readable in
    // `/proc/<pid>/cmdline` and lands in every process listing.
    assert!(
        !args.iter().any(|arg| arg.contains("THE-SECRET")),
        "the identity token reached argv: {args:?}",
    );
    assert_eq!(
        resolved.child_env[ENV_IDENTITY_TOKEN].expose(),
        "eyJhbGciOi.THE-SECRET.sig",
        "the token must reach the child through its environment instead",
    );
}

/// Key mode with `rekor:` uploads to the named instance.
#[test]
fn key_mode_with_rekor_uploads_to_the_named_instance() {
    let config = key_full(
        Ref::File(PathBuf::from("/run/secrets/mirror.key")),
        None,
        Some(Ref::Literal("http://localhost:3000".into())),
    );
    let resolved = resolve_sign(&config, &no_env, &no_files).expect("no secret refs to read");

    assert_eq!(
        sign_push_args(Some(&resolved)),
        [
            "--sign",
            "--key",
            // Verbatim: ocx's own `--key` grammar accepts `file://PATH`, so
            // stripping the scheme here would be a second grammar to keep in
            // step with theirs.
            "file:///run/secrets/mirror.key",
            "--rekor-upload",
            "--rekor-url",
            "http://localhost:3000",
        ],
    );
}

/// No `rekor:` means `--no-rekor-upload`, never silence.
///
/// Silence would let a fleet `[trust.sigstore].rekor_upload = true` publish a
/// private digest to the public transparency log — the one Sigstore setting
/// with a disclosure consequence (S-062).
#[test]
fn key_mode_without_rekor_refuses_the_upload_explicitly() {
    let config = key_full(Ref::Literal("cosign.key".into()), None, None);
    let resolved = resolve_sign(&config, &no_env, &no_files).expect("no refs to resolve");

    assert_eq!(
        sign_push_args(Some(&resolved)),
        ["--sign", "--key", "cosign.key", "--no-rekor-upload"],
    );
}

/// The string form of `key:` is the map form with neither optional set.
#[test]
fn the_bare_key_string_form_matches_the_map_form() {
    let bare = SignConfig {
        keyless: None,
        key: Some(KeyConfig::Reference(Ref::Env("MIRROR_SIGNING_KEY".into()))),
    };
    let full = key_full(Ref::Env("MIRROR_SIGNING_KEY".into()), None, None);

    assert_eq!(
        sign_push_args(Some(&resolve_sign(&bare, &no_env, &no_files).expect("resolves"))),
        sign_push_args(Some(&resolve_sign(&full, &no_env, &no_files).expect("resolves"))),
    );
}

/// No `sign:` block is no words at all — an unsigned mirror's argv is exactly
/// what it was before signing existed.
#[test]
fn no_sign_block_yields_an_empty_tail() {
    assert!(sign_push_args(None).is_empty());
}

// ── C-054: resolution failures name the field and never the value ───────────

/// An unset `env://` variable fails the run, naming both the spec field and
/// the variable an operator has to set (S-061's error case).
#[test]
fn an_unset_variable_names_the_field_and_the_variable() {
    let config = keyless(Some(Ref::Env("SIGSTORE_FULCIO_URL".into())), None, None);

    let error = resolve_sign(&config, &no_env, &no_files).expect_err("an unset endpoint must fail the run");

    let MirrorError::SignMaterialMissing { field, source } = &error else {
        panic!("expected SignMaterialMissing, got {error:?}");
    };
    assert_eq!(field, "sign.keyless.fulcio");
    assert!(source.contains("SIGSTORE_FULCIO_URL"), "got: {source}");
    // 78 rather than 65: the remedy is the runner's configuration, not the
    // spec's syntax, and a scheduled run that keeps exiting 1 tells nobody
    // which.
    assert_eq!(error.kind_exit_code(), ExitCode::ConfigError);
}

/// An unreadable `file://` names the path and the OS reason, and no value —
/// there is none to name, which is exactly what the test has to pin: the
/// message must stay useful without reaching for file contents.
#[test]
fn an_unreadable_file_names_the_path_and_never_its_contents() {
    let config = key_full(
        Ref::Literal("cosign.key".into()),
        Some(Ref::File(PathBuf::from("/run/secrets/passphrase"))),
        None,
    );

    let error = resolve_sign(&config, &no_env, &no_files).expect_err("an unreadable passphrase must fail the run");

    let MirrorError::SignMaterialMissing { field, source } = &error else {
        panic!("expected SignMaterialMissing, got {error:?}");
    };
    assert_eq!(field, "sign.key.passphrase");
    assert!(source.contains("/run/secrets/passphrase"), "got: {source}");
}

/// A file past the cap is refused rather than loaded.
///
/// `file:///dev/zero` is the shape this stops (PKG-04): the values behind a
/// secret ref are hundreds of bytes, so anything at this size is a mistake or
/// an attack, and reading it first to find out is the failure.
#[test]
fn a_file_past_the_cap_is_refused() {
    let oversized: &'static [u8] = Box::leak(vec![b'x'; MAX_SECRET_FILE_BYTES as usize + 1].into_boxed_slice());
    let read = files_of(&[("/run/secrets/passphrase", oversized)]);
    let config = key_full(
        Ref::Literal("cosign.key".into()),
        Some(Ref::File(PathBuf::from("/run/secrets/passphrase"))),
        None,
    );

    let error = resolve_sign(&config, &no_env, &read).expect_err("an oversized secret file must be refused");

    let MirrorError::SignMaterialMissing { field, source } = &error else {
        panic!("expected SignMaterialMissing, got {error:?}");
    };
    assert_eq!(field, "sign.key.passphrase");
    assert!(source.contains("larger than"), "got: {source}");
    assert!(
        !source.contains("xxxx"),
        "the message quoted the file's bytes: {source}"
    );
}

/// Exactly at the cap is accepted — the boundary is `>`, not `>=`.
#[test]
fn a_file_exactly_at_the_cap_is_accepted() {
    let at_cap: &'static [u8] = Box::leak(vec![b'x'; MAX_SECRET_FILE_BYTES as usize].into_boxed_slice());
    let read = files_of(&[("/run/secrets/passphrase", at_cap)]);
    let config = key_full(
        Ref::Literal("cosign.key".into()),
        Some(Ref::File(PathBuf::from("/run/secrets/passphrase"))),
        None,
    );

    let resolved = resolve_sign(&config, &no_env, &read).expect("a file at the cap is within it");
    assert_eq!(
        resolved.child_env[ENV_KEY_PASSWORD].expose().len(),
        MAX_SECRET_FILE_BYTES as usize
    );
}

/// A token file written by `printf '%s\n'` — the ordinary CI shape — must not
/// carry its newline into the child's environment.
#[test]
fn a_secret_file_is_trimmed() {
    let read = files_of(&[("/run/token", b"eyJhbGciOi.token\n".as_slice())]);
    let config = keyless(None, None, Some(Ref::File(PathBuf::from("/run/token"))));

    let resolved = resolve_sign(&config, &no_env, &read).expect("resolves");
    assert_eq!(resolved.child_env[ENV_IDENTITY_TOKEN].expose(), "eyJhbGciOi.token");
}

/// A literal under a secret-class field is refused upstream by
/// `validate_sign_config` (C-051), so this arm is unreachable from a loaded
/// spec. Reached anyway, the value still goes to the child's environment and
/// still never to argv — which is the property that has to hold whatever the
/// validator does.
#[test]
fn a_literal_secret_still_never_reaches_argv() {
    let config = key_full(
        Ref::Literal("cosign.key".into()),
        Some(Ref::Literal("hunter2".into())),
        None,
    );

    let resolved = resolve_sign(&config, &no_env, &no_files).expect("resolves");
    let args = sign_push_args(Some(&resolved));

    assert!(!args.iter().any(|arg| arg.contains("hunter2")), "got: {args:?}");
    assert_eq!(resolved.child_env[ENV_KEY_PASSWORD].expose(), "hunter2");
}

/// `Debug` on the resolved block redacts every secret.
///
/// `ResolvedSign` is reachable from a `tracing` field or an `{:?}` added
/// during a later debugging session; API-02 says the type is what has to stop
/// that, not the discipline of whoever adds the line.
#[test]
fn debug_on_the_resolved_block_redacts_every_secret() {
    let lookup = env_of(&[("CI_TOKEN", "eyJhbGciOi.THE-SECRET.sig")]);
    let config = keyless(None, None, Some(Ref::Env("CI_TOKEN".into())));

    let resolved = resolve_sign(&config, &lookup, &no_files).expect("resolves");
    let rendered = format!("{resolved:?}");

    assert!(!rendered.contains("THE-SECRET"), "Debug leaked the token: {rendered}");
    assert!(rendered.contains("<redacted>"), "got: {rendered}");
}

/// Both mode tags, or neither, is a usage error (64) — `validate_sign_config`
/// already refuses both, so this only pins that the resolver does not silently
/// pick one.
#[test]
fn neither_or_both_mode_tags_is_a_usage_error() {
    for config in [
        SignConfig {
            keyless: None,
            key: None,
        },
        SignConfig {
            keyless: Some(KeylessConfig {
                fulcio: None,
                rekor: None,
                identity_token: None,
            }),
            key: Some(KeyConfig::Reference(Ref::Literal("cosign.key".into()))),
        },
    ] {
        let error = resolve_sign(&config, &no_env, &no_files).expect_err("exactly one mode tag is allowed");
        assert_eq!(error.kind_exit_code(), ExitCode::UsageError, "got {error:?}");
    }
}

// ── C-058 / C-059: the two `package sign` argvs ─────────────────────────────

/// The sweep carries C-052's tail minus `--sign`: `package sign` has nothing
/// to opt into, and passing `--sign` there is an unknown flag (exit 64).
#[test]
fn the_sweep_argv_carries_the_flag_tail_without_the_sign_flag() {
    let resolved = resolve_sign(&keyless(None, None, None), &no_env, &no_files).expect("resolves");

    assert_eq!(
        sweep_args(&resolved, "/work/out.sign-tags", "ghcr.io/ocx-sh/shfmt"),
        [
            "--format",
            "json",
            "--remote",
            "package",
            "sign",
            "--tags-file",
            "/work/out.sign-tags",
            "ghcr.io/ocx-sh/shfmt",
            "--fulcio-url",
            DEFAULT_FULCIO_URL,
            "--rekor-url",
            DEFAULT_REKOR_URL,
        ],
    );
    assert!(
        !sweep_args(&resolved, "/work/out.sign-tags", "ghcr.io/ocx-sh/shfmt").contains(&"--sign".to_string()),
        "`--sign` is a `package push` flag; `package sign` rejects it",
    );
}

/// Both sign argv builders resolve tags against the registry, never the local
/// index.
///
/// `ocx package push` leaves a stale tag -> digest pin in `$OCX_HOME/index`
/// when a later push moves that tag, so a sweep sharing a warm home signs
/// nothing and exits 79 naming a digest the registry no longer serves. Pinned
/// here as its own assertion because `--remote` reads like a redundant default
/// in the argv above and is the first thing a tidy-up would delete.
#[test]
fn both_sign_argv_builders_resolve_tags_remotely() {
    let resolved = resolve_sign(&keyless(None, None, None), &no_env, &no_files).expect("resolves");

    for argv in [
        sweep_args(&resolved, "/work/out.sign-tags", "ghcr.io/ocx-sh/shfmt"),
        sign_reference_args(&resolved, "ghcr.io/ocx-sh/shfmt:3.8.0", None),
    ] {
        let remote = argv.iter().position(|a| a == "--remote").expect("--remote is present");
        let subcommand = argv.iter().position(|a| a == "package").expect("`package` is present");
        assert!(
            remote < subcommand,
            "a global flag must precede the subcommand: {argv:?}"
        );
    }
}

/// `-p` appears exactly when a platform is given, and the flag tail is the
/// same one the sweep uses (C-059).
#[test]
fn a_single_reference_narrows_into_a_platform_only_when_asked() {
    let resolved = resolve_sign(&keyless(None, None, None), &no_env, &no_files).expect("resolves");

    let argv = |platform| sign_reference_args(&resolved, "ghcr.io/ocx-sh/shfmt:3.8.0", platform);

    assert_eq!(
        argv(Some("linux/amd64")),
        [
            "--format",
            "json",
            "--remote",
            "package",
            "sign",
            "-p",
            "linux/amd64",
            "ghcr.io/ocx-sh/shfmt:3.8.0",
            "--fulcio-url",
            DEFAULT_FULCIO_URL,
            "--rekor-url",
            DEFAULT_REKOR_URL,
        ],
    );
    assert!(
        !argv(None).contains(&"-p".to_string()),
        "an index-level sign must not narrow into a platform",
    );
}

/// A failing sign child's message reaches the operator from whichever stream
/// carried it.
///
/// Under `--format json` ocx reports a failure in its stdout envelope, so a
/// sweep whose every tag failed arrives with an empty stderr — and logging
/// stderr alone left `ERROR signing … failed with exit code 79` as the entire
/// account of what went wrong.
#[test]
fn a_failing_sign_child_is_reported_from_whichever_stream_spoke() {
    let envelope = r#"{"exit_code":79,"data":{"tags":[{"tag":"3.8.0","status":"failed"}]}}"#;

    assert_eq!(
        failure_detail("", envelope),
        envelope,
        "stdout is used when stderr is silent"
    );
    assert_eq!(
        failure_detail("  \n ", envelope),
        envelope,
        "whitespace-only stderr is silent"
    );
    assert_eq!(
        failure_detail("no ambient credential provider\n", envelope),
        "no ambient credential provider",
        "stderr still wins when it has something to say",
    );
    assert_eq!(failure_detail("", ""), "", "both silent is not a panic");
}

/// A `skipped` row is the ordinary outcome, not a failure (C-058).
///
/// `push --sign` already signed a bare manifest inline, so the sweep leaving
/// it alone is the design working. Parsing it as a failure would red every run
/// of a repository publishing single-platform packages.
#[test]
fn a_skipped_bare_manifest_row_parses_as_its_own_outcome() {
    // ocx's real envelope, rows nested under `data` — a bare `{"tags":[…]}`
    // fixture is what let the top-level `SweepReport` read as working while it
    // silently deserialized zero rows on every sweep this pipeline ran.
    let stdout = r#"{"schema_version":1,"command":"package sign","exit_code":0,"data":{"tags":[
        {"tag":"3.8.0","status":"completed"},
        {"tag":"latest","status":"skipped"},
        {"tag":"3.8","status":"covered"},
        {"tag":"3","status":"whatever-ocx-adds-next"}
    ]}}"#;

    let envelope: SweepEnvelope = serde_json::from_str(stdout).expect("the sweep report parses");
    let statuses: Vec<&SweptOutcome> = envelope.data.tags.iter().map(|row| &row.status).collect();

    assert_eq!(
        statuses,
        [
            &SweptOutcome::Completed,
            &SweptOutcome::Skipped,
            // `covered` is a status ocx emits today (the cascade dedupe). Left
            // as `Unknown` it would be narrated nowhere: `report_sweep` walks
            // the two named outcomes, so the doc claim that `Unknown` is
            // "reported" only holds for statuses that do not exist yet.
            &SweptOutcome::Covered,
            &SweptOutcome::Unknown,
        ],
        "an unrecognised status must degrade, not fail the parse",
    );
    // Narration only: the child's exit code is the verdict, so an unparseable
    // report must not turn a signed run red.
    report_sweep("not json at all", "ghcr.io/ocx-sh/shfmt");
}

/// The closing sweep is a no-op for a mirror that does not sign.
///
/// The guard `pipeline patch` leans on: it calls [`sweep_index_tags`]
/// unconditionally after its republish loop, so an unsigned mirror must not
/// spawn an `ocx package sign` child — and the absent tags file is the
/// evidence, because a spawned child would have had to be handed one.
#[tokio::test]
async fn a_sweep_without_a_sign_block_writes_nothing_and_spawns_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let tags_file = dir.path().join("nested").join("run.sign-tags");

    sweep_index_tags(None, &["3.8.0".to_string()], "ghcr.io/ocx-sh/shfmt", &tags_file)
        .await
        .expect("an unsigned mirror's sweep is a no-op");
    assert!(!tags_file.exists(), "no `sign:` must not write a tags file");

    // And the same for a signing mirror with nothing to sweep: an empty tags
    // file would cost a child that has no subject to act on.
    let resolved = resolve_sign(&keyless(None, None, None), &no_env, &no_files).expect("no refs to resolve");
    sweep_index_tags(Some(&resolved), &[], "ghcr.io/ocx-sh/shfmt", &tags_file)
        .await
        .expect("an empty tag set is a no-op");
    assert!(!tags_file.exists(), "an empty tag set must not write a tags file");
}

// ── C-054: the child environment ────────────────────────────────────────────

/// `ocx_child_env` applies both secrets, and applies nothing when there is no
/// `sign:` block — a mirror that does not sign must not have its child's
/// `OCX_IDENTITY_TOKEN` cleared or set behind its back.
#[cfg(unix)]
#[test]
fn the_child_environment_carries_the_secrets_and_nothing_else() {
    use std::os::unix::fs::PermissionsExt;

    let lookup = env_of(&[("CI_TOKEN", "eyJhbGciOi.THE-SECRET.sig"), ("PASS", "hunter2")]);
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("env-dump");
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf 'token=%s pass=%s\\n' \"$OCX_IDENTITY_TOKEN\" \"$OCX_KEY_PASSWORD\"\n",
    )
    .expect("script writes");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let token = resolve_sign(
        &keyless(None, None, Some(Ref::Env("CI_TOKEN".into()))),
        &lookup,
        &no_files,
    )
    .expect("resolves");
    let pass = resolve_sign(
        &key_full(Ref::Literal("cosign.key".into()), Some(Ref::Env("PASS".into())), None),
        &lookup,
        &no_files,
    )
    .expect("resolves");

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    // The whole closure runs inside the runtime: `tokio::process::Command`
    // registers its child with the reactor at spawn, so building one outside
    // `block_on` panics before any assertion is reached.
    let dump = |sign: Option<&ResolvedSign>| {
        rt.block_on(async {
            let mut cmd = tokio::process::Command::new(&script);
            // Whatever this test binary inherited must not decide the outcome;
            // removed first, because `env_remove` after `env` would undo it.
            cmd.env_remove(ENV_IDENTITY_TOKEN);
            cmd.env_remove(ENV_KEY_PASSWORD);
            ocx_child_env(&mut cmd, sign);
            let output = cmd.output().await.expect("the script runs");
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        })
    };

    assert_eq!(dump(Some(&token)), "token=eyJhbGciOi.THE-SECRET.sig pass=");
    assert_eq!(dump(Some(&pass)), "token= pass=hunter2");
    assert_eq!(dump(None), "token= pass=", "no `sign:` must set neither variable");
}

// ── The subprocess boundary ─────────────────────────────────────────────────

/// A stub `ocx` on `OCX_BINARY_PIN`, with the body the test wants.
#[cfg(unix)]
fn stub_ocx(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let script = dir.join("stub-ocx");
    std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("script writes");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

/// The child's exit code reaches [`MirrorError::SignFailed`] intact, and its
/// stderr is not swallowed.
///
/// 83 is the case with teeth: `ocx` uses it for a Rekor outage, and it is the
/// one sign failure the push ladder retries. Collapsed to 1 here, the retry
/// never happens and the operator is told the tool failed.
#[cfg(unix)]
#[test]
fn a_failing_sign_child_carries_its_exit_code() {
    let _env_lock = crate::test_support::ocx_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let script = stub_ocx(dir.path(), "echo 'rekor unavailable' >&2\nexit 83");
    // SAFETY: test-only env var, serialized by `ocx_env_lock()`.
    unsafe { std::env::set_var("OCX_BINARY_PIN", &script) };

    let resolved = resolve_sign(&keyless(None, None, None), &no_env, &no_files).expect("resolves");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let error = rt
        .block_on(invoke_sign_reference(&resolved, "ghcr.io/ocx-sh/shfmt:3.8.0", None))
        .expect_err("exit 83 is a failed signature");

    // SAFETY: same lock, same variable — see the set above.
    unsafe { std::env::remove_var("OCX_BINARY_PIN") };

    let MirrorError::SignFailed { target, code } = &error else {
        panic!("expected SignFailed, got {error:?}");
    };
    assert_eq!(*code, 83);
    assert_eq!(target, "ghcr.io/ocx-sh/shfmt:3.8.0");
    assert_eq!(error.kind_exit_code(), ExitCode::TransparencyLogUnavailable);
}

/// A hung sign is killed, not orphaned.
///
/// Same hazard the push leg has: tokio leaves a timed-out child running, so
/// without `kill_on_drop` an abandoned `sign` keeps pushing a referrer
/// manifest at the registry while the run moves on. Observed as a marker file
/// only a survivor lives long enough to write.
#[cfg(unix)]
#[test]
fn a_hung_sign_is_killed_by_its_timeout() {
    let _env_lock = crate::test_support::ocx_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let marker = dir.path().join("survived-the-timeout");
    let script = stub_ocx(dir.path(), &format!("sleep 1\ntouch '{}'", marker.display()));
    // SAFETY: test-only env var, serialized by `ocx_env_lock()`.
    unsafe { std::env::set_var("OCX_BINARY_PIN", &script) };

    let resolved = resolve_sign(&keyless(None, None, None), &no_env, &no_files).expect("resolves");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let started = std::time::Instant::now();
    let error = rt
        .block_on(run_sign(
            &resolved,
            &["--version".to_string()],
            "ghcr.io/ocx-sh/shfmt:3.8.0",
            Duration::from_millis(200),
        ))
        .expect_err("a hung sign must not hang the run");

    // SAFETY: same lock, same variable — see the set above.
    unsafe { std::env::remove_var("OCX_BINARY_PIN") };

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the timeout must bound the wait, took {:?}",
        started.elapsed(),
    );
    // 75, so the push ladder treats a stalled signer as the transient class it
    // is rather than throwing the version away.
    assert_eq!(error.kind_exit_code(), ExitCode::TempFail, "got {error:?}");

    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !marker.exists(),
        "a timed-out sign must be killed, not orphaned against the registry",
    );
}

/// The stub sees the token in its environment and never on its argv.
///
/// The end-to-end form of C-054: the unit tests above pin the resolver and the
/// argv separately, and this pins that `run_sign` wires them the way they were
/// resolved — the seam where a forgotten `ocx_child_env` call leaves a keyless
/// push failing with "no identity" and no clue why, and where a token appended
/// to the argv would be world-readable in `/proc/<pid>/cmdline`.
#[cfg(unix)]
#[test]
fn the_sign_child_receives_the_token_in_its_environment_only() {
    let _env_lock = crate::test_support::ocx_env_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let observed = dir.path().join("observed");
    let script = stub_ocx(
        dir.path(),
        &format!(
            "printf '{{\"tags\":[]}}'\nprintf 'env=[%s] argv=[%s]' \"$OCX_IDENTITY_TOKEN\" \"$*\" > '{}'",
            observed.display(),
        ),
    );
    // SAFETY: test-only env var, serialized by `ocx_env_lock()`.
    unsafe { std::env::set_var("OCX_BINARY_PIN", &script) };

    let lookup = env_of(&[("CI_TOKEN", "eyJhbGciOi.THE-SECRET.sig")]);
    let resolved = resolve_sign(
        &keyless(None, None, Some(Ref::Env("CI_TOKEN".into()))),
        &lookup,
        &no_files,
    )
    .expect("resolves");
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let outcome = rt.block_on(invoke_sign_reference(
        &resolved,
        "ghcr.io/ocx-sh/shfmt:3.8.0",
        Some("linux/amd64"),
    ));

    // SAFETY: same lock, same variable — see the set above.
    unsafe { std::env::remove_var("OCX_BINARY_PIN") };
    outcome.expect("exit 0 is a signed manifest");

    let seen = std::fs::read_to_string(&observed).expect("the stub recorded what it saw");
    assert!(
        seen.contains("env=[eyJhbGciOi.THE-SECRET.sig]"),
        "the child never received the token: {seen}",
    );
    let argv = seen.split("argv=[").nth(1).expect("argv recorded");
    assert!(!argv.contains("THE-SECRET"), "the token reached argv: {seen}");
    assert!(
        argv.contains("-p linux/amd64"),
        "the platform did not reach argv: {seen}"
    );
}
