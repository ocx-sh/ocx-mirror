// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The OCX Authors

//! In-pipeline PEP 751 lock derivation for `source.type: pypi` mirrors
//! (plan_python_mirror_v2 decision A, W1.A2).
//!
//! Two phases, run once per (package, version) in the plan phase:
//!
//! 1. [`materialize_interpreter`] — `ocx package pull`s the pinned
//!    `python.interpreter_package` onto disk and probes the materialized
//!    root for a `bin/python3` executable, so `uv` gets a concrete
//!    `--python` path (a digest/tag reference alone is not something `uv`
//!    can invoke).
//! 2. [`derive_pylock`] — shells `uv pip compile` against that interpreter
//!    into a PEP 751 `pylock.toml`, then relaxes the `requires-python` floor
//!    (uv#15995), stamps a provenance header, and fail-closed re-parses the
//!    result through [`ocx_python::parse_pylock`] before trusting it.
//!
//! Wired into `pipeline plan` (per-candidate invocation, `--locks-dir`
//! persistence — plan_python_mirror_v2 W2.A3) and into `pipeline prepare`'s
//! standalone (no `--plan`) re-derivation path; this module only owns the
//! derivation mechanics.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ocx_python::Pylock;

use crate::pipeline::ocx_cli::{forward_ocx_env, resolve_ocx_binary};
use crate::spec::LockOptions;

/// Shells `ocx --format json package pull <interpreter_package>` and probes
/// the materialized package root for a `bin/python3` executable at any depth
/// (`<root>/content/**/bin/python3`) — the concrete interpreter path `uv`
/// needs for `--python`. A digest/tag reference alone is not runnable.
///
/// # Errors
///
/// Returns a descriptive error string when `ocx` fails to spawn, exits
/// non-zero, its JSON output cannot be parsed or lacks the requested
/// package's entry, or no `bin/python3` is found under the pulled root.
pub(crate) async fn materialize_interpreter(interpreter_package: &str) -> Result<PathBuf, String> {
    let ocx_binary = resolve_ocx_binary()?;
    let mut cmd = tokio::process::Command::new(&ocx_binary);
    cmd.args(["--format", "json", "package", "pull", interpreter_package]);
    forward_ocx_env(&mut cmd);

    let output = cmd.output().await.map_err(|e| format!("failed to spawn ocx: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ocx package pull exited {}: {}", output.status, stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let roots: std::collections::HashMap<String, PathBuf> = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "failed to parse ocx package pull JSON output: {e}\nstdout: {}",
            stdout.trim()
        )
    })?;
    let root = roots
        .get(interpreter_package)
        .ok_or_else(|| format!("ocx package pull output missing entry for '{interpreter_package}'"))?
        .clone();

    let content_dir = root.join("content");
    tokio::task::spawn_blocking(move || find_python3(&content_dir))
        .await
        .map_err(|e| format!("interpreter probe task panicked: {e}"))?
        .ok_or_else(|| format!("no bin/python3 found under '{}/content'", root.display()))
}

/// Recursively searches `dir` for a `bin/python3` file at any depth
/// (depth-first). `DirEntry::file_type()` does not dereference symlinks, so
/// a symlinked directory reports `is_dir() == false` here and is never
/// descended into — cycle-safe without an explicit visited-set.
///
/// Synchronous filesystem walk — call via `spawn_blocking`, never directly
/// from an async context.
fn find_python3(dir: &Path) -> Option<PathBuf> {
    if dir.file_name().is_some_and(|name| name == "bin") {
        let candidate = dir.join("python3");
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir()
            && let Some(found) = find_python3(&entry.path())
        {
            return Some(found);
        }
    }
    None
}

/// Inputs for one [`derive_pylock`] invocation: the target package/version,
/// the mirror-authored [`LockOptions`], and the materialized interpreter
/// path to resolve against. Kept a plain params struct (not a builder) —
/// every field is required and there is exactly one call site
/// (`pipeline plan`, W2.A3).
pub(crate) struct DeriveLockRequest<'a> {
    pub interpreter: &'a Path,
    pub package: &'a str,
    pub version: &'a str,
    pub index: Option<&'a str>,
    pub options: &'a LockOptions,
    pub output_path: &'a Path,
    /// RFC 3339 timestamp stamped into the provenance header. Supplied by
    /// the caller (e.g. the plan run's start time) rather than read from
    /// `SystemTime::now()` here, so derivation stays deterministic and unit
    /// testable.
    pub generated_at: &'a str,
}

/// Resolves the `uv` binary path: `OCX_MIRROR_UV` env var override (mirror
/// convention for hermetic test stubbing, parallel to `ocx`'s own
/// `OCX_BINARY_PIN` in `pipeline::ocx_cli`) — default `"uv"` on `PATH`.
fn resolve_uv_binary() -> PathBuf {
    std::env::var("OCX_MIRROR_UV")
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("uv"))
}

/// Builds the `uv pip compile` argv for one lock derivation. Pure and
/// unit-testable — locks the flag shape without spawning a subprocess.
fn build_uv_compile_args(request: &DeriveLockRequest<'_>) -> Result<Vec<String>, String> {
    let interpreter_str = request
        .interpreter
        .to_str()
        .ok_or_else(|| format!("interpreter path is not valid UTF-8: {}", request.interpreter.display()))?;
    let output_str = request
        .output_path
        .to_str()
        .ok_or_else(|| format!("output path is not valid UTF-8: {}", request.output_path.display()))?;

    let mut args = vec![
        "pip".to_string(),
        "compile".to_string(),
        "-".to_string(),
        "--format".to_string(),
        "pylock.toml".to_string(),
        "-o".to_string(),
        output_str.to_string(),
        "--python".to_string(),
        interpreter_str.to_string(),
    ];

    if request.options.universal {
        args.push("--universal".to_string());
    }
    for excluded in &request.options.exclude {
        args.push("--no-emit-package".to_string());
        args.push(excluded.clone());
    }
    if let Some(index) = request.index {
        args.push("--index-url".to_string());
        args.push(format!("{}/simple", index.trim_end_matches('/')));
    }

    Ok(args)
}

/// Builds the PEP 508 requirement piped to `uv pip compile` on stdin: the
/// package pinned to the exact resolved version, with any configured extras
/// in bracket notation.
fn build_requirement(package: &str, version: &str, extras: &[String]) -> String {
    if extras.is_empty() {
        format!("{package}=={version}")
    } else {
        format!("{package}[{}]=={version}", extras.join(","))
    }
}

/// Spawns `uv pip compile`, writes the requirement to its stdin, and waits
/// for it to write `request.output_path`, bounded by
/// `request.options.timeout_seconds`.
async fn invoke_uv_compile(request: &DeriveLockRequest<'_>) -> Result<(), String> {
    let args = build_uv_compile_args(request)?;
    let requirement = build_requirement(request.package, request.version, &request.options.extras);

    let uv_binary = resolve_uv_binary();
    let mut cmd = tokio::process::Command::new(&uv_binary);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn uv: {e}"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open uv stdin".to_string())?;

    let timeout = Duration::from_secs(request.options.timeout_seconds);
    let run = async {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(requirement.as_bytes())
            .await
            .map_err(|e| format!("failed to write requirement to uv stdin: {e}"))?;
        drop(stdin);
        child
            .wait_with_output()
            .await
            .map_err(|e| format!("failed to wait for uv: {e}"))
    };

    let output = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| format!("uv pip compile timed out after {}s", request.options.timeout_seconds))??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("uv pip compile exited {}: {}", output.status, stderr.trim()));
    }

    Ok(())
}

static REQUIRES_PYTHON_LINE_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r#"(?m)^requires-python\s*=\s*"([^"]*)"\s*$"#).unwrap());

static REQUIRES_PYTHON_RELAX_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r">=(\d+\.\d+)\.\d+").unwrap());

/// Rewrites `>=X.Y.Z` clauses in a `requires-python` specifier string down to
/// `>=X.Y` — works around uv#15995 (uv-derived locks emit an overly strict
/// patch-pinned floor that rejects otherwise-compatible interpreters
/// downstream). Only `>=` clauses are touched; `==`/`<`/etc. are untouched.
fn relax_requires_python(requires_python: &str) -> String {
    REQUIRES_PYTHON_RELAX_RE
        .replace_all(requires_python, ">=$1")
        .into_owned()
}

/// Applies [`relax_requires_python`] to the single top-level `requires-python`
/// line of a raw `pylock.toml` document, leaving everything else in the file
/// byte-for-byte unchanged. A line-based rewrite (not a full TOML
/// parse/reserialize) so uv's own formatting and comments survive untouched.
fn relax_requires_python_in_lock(contents: &str) -> String {
    REQUIRES_PYTHON_LINE_RE
        .replace(contents, |caps: &regex::Captures<'_>| {
            format!("requires-python = \"{}\"", relax_requires_python(&caps[1]))
        })
        .into_owned()
}

/// Builds the provenance comment header stamped onto every derived lock —
/// TOML comments are valid at the start of any document, so this composes
/// cleanly with the relaxed lock body.
fn provenance_header(request: &DeriveLockRequest<'_>) -> String {
    format!(
        "# Generated by ocx-mirror (source.type: pypi lock derivation).\n\
         # package: {}\n\
         # version: {}\n\
         # generated_at: {}\n\
         #\n",
        request.package, request.version, request.generated_at
    )
}

/// Derives a PEP 751 lock for `request.package==request.version` against
/// `request.interpreter`, post-processes it (uv#15995 relax + provenance
/// header), and fail-closed re-parses the result through
/// [`ocx_python::parse_pylock`] — a derived lock this crate cannot re-parse
/// is never handed to the plan/prepare phases as if it were trustworthy.
///
/// # Errors
///
/// Returns a descriptive error string when `uv` fails to spawn, exits
/// non-zero, times out, the output file cannot be read/written, or the
/// post-processed lock fails to re-parse.
pub(crate) async fn derive_pylock(request: &DeriveLockRequest<'_>) -> Result<Pylock, String> {
    invoke_uv_compile(request).await?;

    let raw = tokio::fs::read_to_string(request.output_path).await.map_err(|e| {
        format!(
            "failed to read uv-derived lock '{}': {e}",
            request.output_path.display()
        )
    })?;

    let relaxed = relax_requires_python_in_lock(&raw);
    let final_contents = format!("{}{relaxed}", provenance_header(request));

    tokio::fs::write(request.output_path, &final_contents)
        .await
        .map_err(|e| format!("failed to write derived lock '{}': {e}", request.output_path.display()))?;

    ocx_python::parse_pylock(&final_contents).map_err(|e| {
        format!(
            "derived lock for '{}=={}' failed to re-parse: {e}",
            request.package, request.version
        )
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Serializes tests that mutate `OCX_BINARY_PIN` / `OCX_MIRROR_UV` —
    /// process-global env vars, unsafe to set concurrently across tests in
    /// the same process (adapted from `command/package/pipeline/push.rs`'s
    /// `job_url_env_lock`, but async: the guard here must stay held across
    /// the subprocess `.await` under test, so this uses `tokio::sync::Mutex`
    /// rather than `std::sync::Mutex` — the one case quality-rust carves out
    /// for holding a lock across an await point).
    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    fn write_executable_script(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write script");
        let mut perms = std::fs::metadata(path).expect("stat script").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms).expect("chmod script");
    }

    fn sample_options() -> LockOptions {
        LockOptions {
            universal: true,
            extras: Vec::new(),
            exclude: Vec::new(),
            timeout_seconds: 300,
        }
    }

    // ── find_python3 ─────────────────────────────────────────────────────

    #[test]
    fn find_python3_locates_nested_bin_directory() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("python/install/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3"), "").unwrap();

        let found = find_python3(root.path()).expect("python3 found");
        assert_eq!(found, bin.join("python3"));
    }

    #[test]
    fn find_python3_returns_none_when_absent() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("lib/python3.13")).unwrap();

        assert!(find_python3(root.path()).is_none());
    }

    #[test]
    fn find_python3_ignores_bin_directory_missing_the_binary() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        std::fs::write(root.path().join("bin/pip3"), "").unwrap();

        assert!(find_python3(root.path()).is_none());
    }

    // ── materialize_interpreter ──────────────────────────────────────────

    #[tokio::test]
    async fn materialize_interpreter_resolves_pulled_python3() {
        let _guard = env_lock().await;
        let interpreter_root = tempfile::tempdir().unwrap();
        let bin = interpreter_root.path().join("content/python/install/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("python3"), "").unwrap();

        let scripts_dir = tempfile::tempdir().unwrap();
        let stub = scripts_dir.path().join("ocx");
        write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho '{{\"ocx.sh/python/cpython:3.13.1\": \"{}\"}}'\n",
                interpreter_root.path().display()
            ),
        );

        // SAFETY: test-only env var, serialized by `env_lock()`.
        unsafe {
            std::env::set_var("OCX_BINARY_PIN", &stub);
        }
        let result = materialize_interpreter("ocx.sh/python/cpython:3.13.1").await;
        unsafe {
            std::env::remove_var("OCX_BINARY_PIN");
        }

        assert_eq!(result.unwrap(), bin.join("python3"));
    }

    #[tokio::test]
    async fn materialize_interpreter_surfaces_nonzero_exit() {
        let _guard = env_lock().await;
        let scripts_dir = tempfile::tempdir().unwrap();
        let stub = scripts_dir.path().join("ocx");
        write_executable_script(&stub, "#!/bin/sh\necho 'pull failed' >&2\nexit 1\n");

        // SAFETY: test-only env var, serialized by `env_lock()`.
        unsafe {
            std::env::set_var("OCX_BINARY_PIN", &stub);
        }
        let result = materialize_interpreter("ocx.sh/python/cpython:3.13.1").await;
        unsafe {
            std::env::remove_var("OCX_BINARY_PIN");
        }

        let err = result.unwrap_err();
        assert!(err.contains("pull failed"), "got: {err}");
    }

    #[tokio::test]
    async fn materialize_interpreter_rejects_missing_json_entry() {
        let _guard = env_lock().await;
        let scripts_dir = tempfile::tempdir().unwrap();
        let stub = scripts_dir.path().join("ocx");
        write_executable_script(&stub, "#!/bin/sh\necho '{\"some-other-package\": \"/tmp/x\"}'\n");

        // SAFETY: test-only env var, serialized by `env_lock()`.
        unsafe {
            std::env::set_var("OCX_BINARY_PIN", &stub);
        }
        let result = materialize_interpreter("ocx.sh/python/cpython:3.13.1").await;
        unsafe {
            std::env::remove_var("OCX_BINARY_PIN");
        }

        let err = result.unwrap_err();
        assert!(err.contains("missing entry"), "got: {err}");
    }

    #[tokio::test]
    async fn materialize_interpreter_rejects_root_without_python3() {
        let _guard = env_lock().await;
        let interpreter_root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(interpreter_root.path().join("content/lib")).unwrap();

        let scripts_dir = tempfile::tempdir().unwrap();
        let stub = scripts_dir.path().join("ocx");
        write_executable_script(
            &stub,
            &format!(
                "#!/bin/sh\necho '{{\"ocx.sh/python/cpython:3.13.1\": \"{}\"}}'\n",
                interpreter_root.path().display()
            ),
        );

        // SAFETY: test-only env var, serialized by `env_lock()`.
        unsafe {
            std::env::set_var("OCX_BINARY_PIN", &stub);
        }
        let result = materialize_interpreter("ocx.sh/python/cpython:3.13.1").await;
        unsafe {
            std::env::remove_var("OCX_BINARY_PIN");
        }

        let err = result.unwrap_err();
        assert!(err.contains("no bin/python3 found"), "got: {err}");
    }

    // ── build_uv_compile_args / build_requirement ────────────────────────

    #[test]
    fn build_uv_compile_args_includes_universal_and_index() {
        let options = LockOptions {
            universal: true,
            extras: Vec::new(),
            exclude: vec!["setuptools".to_string()],
            timeout_seconds: 60,
        };
        let request = DeriveLockRequest {
            interpreter: Path::new("/opt/python/bin/python3"),
            package: "pycowsay",
            version: "1.0.0",
            index: Some("https://example.com/pypi/"),
            options: &options,
            output_path: Path::new("/work/pylock.toml"),
            generated_at: "2026-07-05T00:00:00Z",
        };

        let args = build_uv_compile_args(&request).unwrap();

        assert!(args.contains(&"--universal".to_string()));
        let index_flag = args
            .iter()
            .position(|a| a == "--index-url")
            .expect("--index-url present");
        assert_eq!(args[index_flag + 1], "https://example.com/pypi/simple");
        let exclude_flag = args
            .iter()
            .position(|a| a == "--no-emit-package")
            .expect("--no-emit-package present");
        assert_eq!(args[exclude_flag + 1], "setuptools");
        let python_flag = args.iter().position(|a| a == "--python").expect("--python present");
        assert_eq!(args[python_flag + 1], "/opt/python/bin/python3");
        let output_flag = args.iter().position(|a| a == "-o").expect("-o present");
        assert_eq!(args[output_flag + 1], "/work/pylock.toml");
    }

    #[test]
    fn build_uv_compile_args_omits_universal_and_index_when_unset() {
        let options = LockOptions {
            universal: false,
            extras: Vec::new(),
            exclude: Vec::new(),
            timeout_seconds: 60,
        };
        let request = DeriveLockRequest {
            interpreter: Path::new("/opt/python/bin/python3"),
            package: "pycowsay",
            version: "1.0.0",
            index: None,
            options: &options,
            output_path: Path::new("/work/pylock.toml"),
            generated_at: "2026-07-05T00:00:00Z",
        };

        let args = build_uv_compile_args(&request).unwrap();

        assert!(!args.contains(&"--universal".to_string()));
        assert!(!args.contains(&"--index-url".to_string()));
    }

    #[test]
    fn build_requirement_adds_extras_bracket() {
        assert_eq!(build_requirement("pycowsay", "1.0.0", &[]), "pycowsay==1.0.0");
        assert_eq!(
            build_requirement("pycowsay", "1.0.0", &["extra1".to_string(), "extra2".to_string()]),
            "pycowsay[extra1,extra2]==1.0.0"
        );
    }

    // ── relax_requires_python ─────────────────────────────────────────────

    #[test]
    fn relax_requires_python_drops_patch_from_gte_clause() {
        assert_eq!(relax_requires_python(">=3.9.1"), ">=3.9");
    }

    #[test]
    fn relax_requires_python_leaves_other_operators_untouched() {
        assert_eq!(relax_requires_python(">=3.9.1,<4.0.0"), ">=3.9,<4.0.0");
        assert_eq!(relax_requires_python("==3.11.2"), "==3.11.2");
    }

    #[test]
    fn relax_requires_python_in_lock_only_touches_requires_python_line() {
        let raw = "lock-version = \"1.0\"\nrequires-python = \">=3.9.1\"\n\n[[packages]]\nname = \"six\"\nversion = \"1.16.0\"\n";
        let relaxed = relax_requires_python_in_lock(raw);
        assert!(relaxed.contains("requires-python = \">=3.9\""));
        assert!(relaxed.contains("name = \"six\""));
        assert!(!relaxed.contains(">=3.9.1"));
    }

    // ── derive_pylock (uv stub) ───────────────────────────────────────────

    const STUB_LOCK_BODY: &str = r#"lock-version = "1.0"
requires-python = ">=3.9.1"

[[packages]]
name = "pycowsay"
version = "1.0.0"

[[packages.wheels]]
name = "pycowsay-1.0.0-py3-none-any.whl"
url = "https://example.com/pycowsay-1.0.0-py3-none-any.whl"
hashes = { sha256 = "aaaa" }
"#;

    /// Writes a stub `uv` that: consumes stdin (proving the requirement was
    /// piped), locates the `-o <path>` argument, and writes `body` there.
    fn write_uv_stub(path: &Path, body: &str, exit_code: u32) {
        let script = format!(
            "#!/bin/sh\n\
             cat > /dev/null\n\
             prev=\"\"\n\
             outfile=\"\"\n\
             for arg in \"$@\"; do\n\
             \x20 if [ \"$prev\" = \"-o\" ]; then outfile=\"$arg\"; fi\n\
             \x20 prev=\"$arg\"\n\
             done\n\
             if [ -n \"$outfile\" ]; then cat > \"$outfile\" <<'LOCKEOF'\n{body}LOCKEOF\n\
             fi\n\
             exit {exit_code}\n"
        );
        write_executable_script(path, &script);
    }

    #[tokio::test]
    async fn derive_pylock_relaxes_stamps_and_reparses() {
        let _guard = env_lock().await;
        let scripts_dir = tempfile::tempdir().unwrap();
        let stub = scripts_dir.path().join("uv");
        write_uv_stub(&stub, STUB_LOCK_BODY, 0);

        let work_dir = tempfile::tempdir().unwrap();
        let output_path = work_dir.path().join("pylock.toml");
        let options = sample_options();
        let request = DeriveLockRequest {
            interpreter: Path::new("/opt/python/bin/python3"),
            package: "pycowsay",
            version: "1.0.0",
            index: None,
            options: &options,
            output_path: &output_path,
            generated_at: "2026-07-05T00:00:00Z",
        };

        // SAFETY: test-only env var, serialized by `env_lock()`.
        unsafe {
            std::env::set_var("OCX_MIRROR_UV", &stub);
        }
        let result = derive_pylock(&request).await;
        unsafe {
            std::env::remove_var("OCX_MIRROR_UV");
        }

        let lock = result.expect("derivation succeeds");
        assert_eq!(lock.packages.len(), 1);
        assert_eq!(lock.packages[0].name, "pycowsay");
        assert_eq!(lock.requires_python.as_deref(), Some(">=3.9"));

        let on_disk = std::fs::read_to_string(&output_path).unwrap();
        assert!(on_disk.contains("# Generated by ocx-mirror"));
        assert!(on_disk.contains("# package: pycowsay"));
        assert!(on_disk.contains("requires-python = \">=3.9\""));
        assert!(!on_disk.contains(">=3.9.1"));
    }

    #[tokio::test]
    async fn derive_pylock_surfaces_uv_failure() {
        let _guard = env_lock().await;
        let scripts_dir = tempfile::tempdir().unwrap();
        let stub = scripts_dir.path().join("uv");
        write_executable_script(
            &stub,
            "#!/bin/sh\ncat > /dev/null\necho 'resolution failed' >&2\nexit 1\n",
        );

        let work_dir = tempfile::tempdir().unwrap();
        let output_path = work_dir.path().join("pylock.toml");
        let options = sample_options();
        let request = DeriveLockRequest {
            interpreter: Path::new("/opt/python/bin/python3"),
            package: "pycowsay",
            version: "1.0.0",
            index: None,
            options: &options,
            output_path: &output_path,
            generated_at: "2026-07-05T00:00:00Z",
        };

        // SAFETY: test-only env var, serialized by `env_lock()`.
        unsafe {
            std::env::set_var("OCX_MIRROR_UV", &stub);
        }
        let result = derive_pylock(&request).await;
        unsafe {
            std::env::remove_var("OCX_MIRROR_UV");
        }

        let err = result.unwrap_err();
        assert!(err.contains("resolution failed"), "got: {err}");
    }

    #[tokio::test]
    async fn derive_pylock_fails_closed_on_unparseable_lock() {
        let _guard = env_lock().await;
        // A sdist-only package (no [[packages.wheels]]) parses as valid TOML
        // but is rejected by ocx_python::parse_pylock — the derived lock
        // must not be trusted even though uv "succeeded".
        let bad_body = "lock-version = \"1.0\"\n\n[[packages]]\nname = \"uwsgi\"\nversion = \"2.0.24\"\n";
        let scripts_dir = tempfile::tempdir().unwrap();
        let stub = scripts_dir.path().join("uv");
        write_uv_stub(&stub, bad_body, 0);

        let work_dir = tempfile::tempdir().unwrap();
        let output_path = work_dir.path().join("pylock.toml");
        let options = sample_options();
        let request = DeriveLockRequest {
            interpreter: Path::new("/opt/python/bin/python3"),
            package: "uwsgi",
            version: "2.0.24",
            index: None,
            options: &options,
            output_path: &output_path,
            generated_at: "2026-07-05T00:00:00Z",
        };

        // SAFETY: test-only env var, serialized by `env_lock()`.
        unsafe {
            std::env::set_var("OCX_MIRROR_UV", &stub);
        }
        let result = derive_pylock(&request).await;
        unsafe {
            std::env::remove_var("OCX_MIRROR_UV");
        }

        let err = result.unwrap_err();
        assert!(err.contains("failed to re-parse"), "got: {err}");
    }

    #[tokio::test]
    async fn derive_pylock_times_out_on_hung_uv() {
        let _guard = env_lock().await;
        let scripts_dir = tempfile::tempdir().unwrap();
        let stub = scripts_dir.path().join("uv");
        write_executable_script(&stub, "#!/bin/sh\ncat > /dev/null\nsleep 5\n");

        let work_dir = tempfile::tempdir().unwrap();
        let output_path = work_dir.path().join("pylock.toml");
        let options = LockOptions {
            universal: true,
            extras: Vec::new(),
            exclude: Vec::new(),
            timeout_seconds: 1,
        };
        let request = DeriveLockRequest {
            interpreter: Path::new("/opt/python/bin/python3"),
            package: "pycowsay",
            version: "1.0.0",
            index: None,
            options: &options,
            output_path: &output_path,
            generated_at: "2026-07-05T00:00:00Z",
        };

        // SAFETY: test-only env var, serialized by `env_lock()`.
        unsafe {
            std::env::set_var("OCX_MIRROR_UV", &stub);
        }
        let result = derive_pylock(&request).await;
        unsafe {
            std::env::remove_var("OCX_MIRROR_UV");
        }

        let err = result.unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
    }
}
