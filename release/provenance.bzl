"""`release_provenance` — the release build's stand-in for build.rs's real branch.

One rustc env file (`KEY=VALUE` lines) for `//:ocx_mirror`'s `rustc_env_files`
under `--define=ocx_mirror_provenance=release`. Two sources, as build.rs has:

* The workspace status (`ctx.info_file`, `bazel-out/stable-status.txt`) —
  `release/workspace_status.py` prints every git, CI and build-channel value
  as `STABLE_OCX_MIRROR_<VAR> <value>`; the prefix is stripped here. A value
  the script did not print is absent from the file, so `option_env!` sees
  None and `--json version` omits the field, exactly as build.rs's
  pass-through does. rules_rust's own stamping (`stamp` + `{KEY}` in an env
  file) cannot express that: an unknown `{KEY}` stays in the value verbatim.
* The rust toolchain of the target configuration — the triple and rustc
  version vergen's `CargoBuilder`/`RustcBuilder` read from cargo, and the
  debug flag from the compilation mode (`dbg` = cargo's `debug = true`).

Stable keys only: Bazel keys an action on `stable-status.txt` and pretends
`volatile-status.txt` never changes, so a value read from the volatile half
would go stale in the local action cache. The action is `no-remote-cache` —
ocx's stamping ruling: nothing that read the workspace status reaches the
shared cache. The compile it feeds is keyed on this file's content.
"""

visibility("private")

def _release_provenance_impl(ctx):
    toolchain = ctx.toolchains["@rules_rust//rust:toolchain_type"]
    out = ctx.actions.declare_file(ctx.label.name + ".env")
    ctx.actions.run_shell(
        inputs = [ctx.info_file],
        outputs = [out],
        arguments = [
            ctx.info_file.path,
            out.path,
            toolchain.target_triple.str,
            toolchain.version,
            "true" if ctx.var["COMPILATION_MODE"] == "dbg" else "false",
        ],
        command = """
set -eu
sed -n 's/^STABLE_OCX_MIRROR_\\([A-Za-z0-9_]*\\) \\(.*\\)$/\\1=\\2/p' "$1" > "$2"
printf 'VERGEN_CARGO_TARGET_TRIPLE=%s\\nVERGEN_RUSTC_SEMVER=%s\\nVERGEN_CARGO_DEBUG=%s\\n' "$3" "$4" "$5" >> "$2"
""",
        execution_requirements = {"no-remote-cache": "1"},
        mnemonic = "OcxMirrorProvenance",
        progress_message = "Writing release provenance for %{label}",
    )
    return [DefaultInfo(files = depset([out]))]

release_provenance = rule(
    implementation = _release_provenance_impl,
    doc = "The release build's rustc env file: workspace-status provenance plus the target toolchain's triple and rustc.",
    toolchains = ["@rules_rust//rust:toolchain_type"],
)
