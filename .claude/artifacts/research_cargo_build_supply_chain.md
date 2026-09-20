# Research: build-time supply-chain security for `source.type: cargo`

<!--
Technology Landscape Research
Filename: artifacts/research_cargo_build_supply_chain.md
Owner: Researcher (worker-researcher)
Handoff to: Architect (/architect), Swarm Plan (/swarm-plan)
Related Skills: architect, swarm-plan

Purpose: Persist tech landscape findings to inform ADRs, plans, design decisions.
Artifacts decay — check dates before trusting findings.
-->

## Metadata

**Date:** 2026-09-17
**Domain:** security
**Triggered by:** ADR cargo source kind (hex-arch-cargo-20260917) — `ocx-mirror` would run
`cargo install <crate>@<ver> --locked --root <dir>` on GitHub-hosted runners (six native
targets) and publish `<root>/bin/*` as OCX packages.
**Expires:** 2027-03-17

Builds on [`research_mirror_supply_chain.md`](./research_mirror_supply_chain.md) (2026-08-13,
TUF taxonomy for a *rewriting mirror* — that artifact is about the published index being the
trust root; this one is about the *build step that produces the bytes* the index will point
to). Scoped by [`security-threat-model.md`](../rules/security-threat-model.md): the attacker
is upstream (crates.io index content, the `.crate` tarball, transitive `build.rs`/proc-macro
code) and the network; **the GitHub-hosted runner executing the build is trusted** per owner
ruling — this research is about what compensating control that ruling still needs, not about
whether the ruling is sound.

## Direct Answer

Owner ruling (2026-09-17): `build.rs`/proc-macro execution on a secretless, ephemeral runner is
in-scope-and-accepted, with a two-job split (build has no registry credential; a separate
`push` job does) as the compensating control. **This is exactly the shape the ecosystem
converged on independently**: nobody sandboxes Rust build scripts in production today (rustc
has no capability model for `build.rs`; the only real mitigations are *keep it credential-free*
and *isolate its blast radius*), but everyone who publishes from CI enforces job-level secret
separation, because that is the one control that actually survives arbitrary code execution in
the build step. crates.io's own 2026 Trusted Publishing rollout and GitHub's SLSA L2/L3
generators are both instances of the same pattern: OIDC-scoped, short-lived, single-job
credentials instead of a long-lived secret sitting next to `cargo build`. The mirror should
adopt the same shape for its own `push` credential and additionally treat the *rebuilt* binary
as **provenance-bearing but not reproducible** — SLSA provenance (in-toto predicate) is the
honest artifact to attach; byte-reproducibility is not realistic to promise for arbitrary
third-party crates in 2026.

## Technology Landscape

### Established (proven, widely accepted)

| Tool/Pattern | Status | Notes |
|---|---|---|
| Two-job split: build (no secrets) → push (holds credential) | Standard SLSA L2/L3 shape on GHA | [SLSA Build L2/L3](https://slsa.dev/spec/v1.2/levels), realized via `slsa-github-generator`'s BYOB reusable-workflow pattern — a **reusable workflow** runs in its own isolated job the caller cannot tamper with, unlike a composite action which runs inside the caller's job ([BYOB.md](https://github.com/slsa-framework/slsa-github-generator/blob/main/BYOB.md)) |
| `cargo audit` / `cargo deny` as CI gates | Mature, widely deployed | RustSec's own interface tool; a combined `cargo vet` + `cargo audit --deny warnings` + `cargo deny check` gate is reported to have caught RUSTSEC-2024-0003 within 48h in adopter repos ([systemshardening.com summary](https://www.systemshardening.com/articles/cicd/rust-cargo-supply-chain-security/)) |
| Fixed-output / hash-pinned network fetch (Nix model) | Mature in Nix/NixOS, not in cargo | Nix sandboxes builds with **no network** by default; the only escape hatch is a fixed-output derivation where the *output hash* is declared up front, so the network hole is only ever opened for a pinned, verifiable result ([NixOS build sandbox](https://ayats.org/blog/nix-tuto-2), [fixed-output derivations](https://phip1611.de/blog/accessing-network-from-a-nix-derivation/)). Cargo has no equivalent for `build.rs` network access — a build script network-calls with the ambient credentials of whatever job invoked it. |
| SHA-256 `cksum` over the `.crate` tarball, HTTPS-only index | crates.io production since the sparse-index migration (Rust 1.70, 2023) | [Registry Index — Cargo Book](https://doc.rust-lang.org/cargo/reference/registry-index.html); `cksum` is documented as "A SHA256 checksum of the `.crate` file." Integrity of *transport and storage*, not authorship — no signature over the index entry itself. |
| `Cargo.lock` shipped in every published crate | Cargo 1.84 (Dec 2024) onward, all published crates unconditionally | Previously only crates with a binary/example shipped one; now every `.crate` carries a lockfile the publisher resolved against ([cargo#14815](https://github.com/rust-lang/cargo/pull/14815)). `--locked` forces cargo to use exactly that file instead of re-resolving. |

### Trending (gaining momentum, 2025–2026)

| Tool/Pattern | Adoption Signal | Key Benefit | Relevance to ocx-mirror |
|---|---|---|---|
| crates.io Trusted Publishing (OIDC) | Shipped 2025, GitLab CI support added early 2026 ([crates.io dev update, 2026-01-21](https://blog.rust-lang.org/2026/01/21/crates-io-development-update/)) | Replaces long-lived `CARGO_REGISTRY_TOKEN` with a short-lived token minted per-run from a GitHub OIDC ID token, scoped to (org/repo, workflow filename, optional environment) — [RFC 3691](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html) | Direct model for the mirror's own `push` job credential: OIDC-to-registry instead of a stored PAT/token, scoped to the one workflow file that is allowed to push |
| GitHub Artifact Attestations (`actions/attest-build-provenance`) | GA, actively recommended alongside SLSA generators | Signs an in-toto/SLSA provenance statement over build outputs using Sigstore/Fulcio+Rekor, verifiable with `gh attestation verify` | Same signing stack the mirror already uses (`ocx package push --sign`, keyless Fulcio/Rekor per `adr_mirror_signing.md`) — the *predicate content* differs (source-build provenance vs. OCI-copy attestation) but the trust root is identical, so no new PKI to bootstrap |
| cargo-vet / cargo-barbican-style policy gates | Mozilla's cargo-vet has multi-org shared audit adoption; newer tools (cargo-barbican, 2026) layer minimum-release-age and "reviewed-target" policy on top | Human-reviewed or age-gated admission *before* `cargo install` ever executes a crate's `build.rs` | Optional pre-execution gate the mirror could offer operators (see Recommendation) — not a substitute for the two-job split, since a first-time crate still runs unreviewed code somewhere |

### Declining / cautionary

| Tool/Pattern | Signal | Avoid Because |
|---|---|---|
| Trusting `actions/upload-artifact` cross-job as a tamper barrier | Documented bypass, GitHub says by design | A job in the *same workflow* can call the upload-artifact internals to overwrite another job's artifact (delete+recreate gets a new ID, but nothing stops an adjacent job from doing that before the consumer downloads it) — [Imre Rad, "GitHub artifact immutability is a lie"](https://irsl.medium.com/github-artifact-immutability-is-a-lie-9b6244095694). The immutability guarantee is per-artifact-once-finalized, not "other jobs in this workflow cannot touch it." |
| Byte-reproducible builds as a hard requirement for a mirror | Active RFC-level effort (`trim-paths`, [RFC 3127](https://rust-lang.github.io/rfcs/3127-trim-paths.html)) but no ecosystem-wide guarantee | Reproducible-builds.org's own [Rust page](https://reproducible-builds.org/docs/rust/) documents `SOURCE_DATE_EPOCH`, `--remap-path-prefix`/`--trim-paths` as *available knobs*, not a default outcome — build timestamps embedded by `build.rs` via `env!`/`option_env!`, absolute paths, and non-deterministic proc-macro codegen (e.g., `HashMap` iteration order surfacing in generated code) remain unbounded per-crate failure modes the mirror cannot fix from outside |

## Design Patterns Worth Considering

- **Two-job split with a non-composite reusable workflow boundary** — the build job(s) (six
  native targets) hold zero registry credentials and only ever touch the untrusted crate
  source; a single downstream `push` job, gated on all build jobs succeeding, holds the
  registry credential and does nothing but verify artifact hashes and push. This is the exact
  shape SLSA calls out as the L1→L2 boundary on GitHub Actions specifically because a
  **composite action runs inside the caller's job** (so the caller *can* tamper with it) while
  a **reusable workflow gets its own isolated job** the caller cannot reach into
  ([BYOB.md](https://github.com/slsa-framework/slsa-github-generator/blob/main/BYOB.md)).
  Used by: every `slsa-github-generator` builder, GitHub's own `attest-build-provenance` docs.
- **OIDC-minted, single-purpose credential instead of a stored secret** — crates.io Trusted
  Publishing: the ID token's claims (repo, workflow filename, optional environment) are
  checked server-side before a short-lived token is minted; "workflow file name to be defined
  in order to limit the attack surface" ([RFC 3691](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html)).
  Directly transplantable to whatever registry the mirror's `push` job authenticates to, if
  that registry supports OIDC federation (most OCI registries with GitHub OIDC support do,
  e.g. GHCR, ECR, GAR).
- **Fixed-output escape hatch for necessary network access** — Nix's answer to "sometimes a
  build genuinely needs the network" is never "leave the sandbox open," it's "pin the output
  hash and open a narrow, single-purpose hole for exactly that fetch"
  ([phip1611.de](https://phip1611.de/blog/accessing-network-from-a-nix-derivation/)). Cargo has
  no such primitive for `build.rs`, which is precisely why the compensating control has to live
  one layer up (job isolation), not inside the build.
- **Provenance as a statement about the build, not a claim of byte-identity** — SLSA v1
  provenance's `buildDefinition` (what was built: source digest, builder-declared parameters)
  + `runDetails` (how: builder identity, timestamps) is designed to be attestable even when the
  build is not reproducible — it says "builder X built input Y with parameters Z," not "anyone
  re-running this gets the same bytes" ([SLSA Provenance v1.2](https://slsa.dev/spec/v1.2/build-provenance)).
  This is the right shape for the mirror's rebuilt-artifact claim.

## Key Findings

1. **`build.rs`/proc-macro execution is unsandboxed by design across the whole ecosystem, and
   this is not a cargo-specific gap** — cargo runs build scripts "with your privileges,
   environment, and network... with no sandbox by default," and proc-macros have the same
   unrestricted file/network access; sandboxing (WASM-based) is discussed but not shipped
   ([Rust Internals thread](https://internals.rust-lang.org/t/sandbox-build-rs-and-proc-macros/16345)).
   docs.rs runs builds inside a container without network access as its compensating control,
   not a build.rs-specific sandbox — i.e. the same "isolate the blast radius" answer this ADR
   is converging on, at a different layer.
2. **Named incidents confirm the exfiltration path is real and has hit CI secrets specifically**:
   `rustdecimal` (2022) — a typosquat of `rust_decimal` whose `Decimal::new` checked for
   `GITLAB_CI` and downloaded/executed a second-stage binary from `/tmp/git-updater.bin` when
   present, i.e. it specifically targeted CI environments
   ([RUSTSEC-2022-0042](https://rustsec.org/advisories/RUSTSEC-2022-0042.html),
   [official advisory](https://blog.rust-lang.org/2022/05/10/malicious-crate-rustdecimal/)).
   `CrateDepression` (2022) — a malicious `rustdecimal`-adjacent campaign that dropped a Go
   payload into cloud CI pipelines specifically to harvest CI secrets
   ([SentinelOne](https://www.sentinelone.com/labs/cratedepression-rust-supply-chain-attack-infects-cloud-ci-pipelines-with-go-malware/)).
   A 2026 campaign (`chrono_anchor`, `time_calibrator`, and others impersonating `timeapi.io`)
   POSTed `.env` file contents — described as potentially containing "cloud services, internal
   databases, GitHub and registry tokens, and sometimes signing material" — to an
   attacker-controlled domain (source: aggregator coverage, flagged below as lower-confidence
   secondary reporting, cite with caution: [Socket.dev writeup](https://socket.dev/blog/5-malicious-rust-crates-posed-as-time-utilities-to-exfiltrate-env-files)).
3. **crates.io itself moved to OIDC exactly because long-lived publish tokens are the
   recurring failure mode** — Trusted Publishing (2025, GitLab support added January 2026)
   replaces stored `CARGO_REGISTRY_TOKEN` secrets with per-run tokens scoped to repo + workflow
   filename + optional environment, "typically valid for less than an hour"
   ([RFC 3691](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html),
   [dev update 2026-01-21](https://blog.rust-lang.org/2026/01/21/crates-io-development-update/)).
   The RFC explicitly scopes out provenance/signing as a separate concern — Trusted Publishing
   is credential hygiene, not an attestation of what was built.
4. **`actions/upload-artifact` v4's "immutability" does not mean inter-job tamper-proof** — a
   job within the *same workflow* can call the upload-artifact action's own internals to
   overwrite another job's artifact before it's consumed; GitHub has stated this is by design
   and not planned to be fixed
   ([Imre Rad, Medium](https://irsl.medium.com/github-artifact-immutability-is-a-lie-9b6244095694)).
   **This directly threatens the two-job split's threat model**: if the six build-target
   outputs cross the build→push boundary via `actions/upload-artifact`, a compromised build job
   for target A could, in principle, tamper with target B's artifact before the push job reads
   it — the isolation the split is meant to buy is weaker than "different job = safe" if the
   hand-off channel is a same-workflow artifact store. The push job must re-verify each
   artifact's hash against what the build job *reported* (job output/step summary, not just
   "whatever is in the artifact store"), or use per-target job outputs plus a checksum
   manifest signed/reported by the isolated generator job pattern SLSA uses.
5. **SLSA Build L2 is the natural ceiling for "GitHub-hosted, no self-hosted isolation
   guarantee"; L3 requires more than a two-job split** — L1 is provenance-exists-but-forgeable;
   L2 requires the provenance to be generated by a hosted platform in a way the caller cannot
   forge (satisfied by a reusable-workflow-isolated job); L3 additionally requires the build
   platform to "prevent one build run from affecting another" and "keep signing secrets out of
   reach of user-defined build steps"
   ([SLSA Build Levels](https://slsa.dev/spec/v1.2/levels)) — GitHub-hosted runners give L2 for
   free via the reusable-workflow pattern but do not on their own give L3's cross-run isolation
   guarantee (ephemeral VM per job is necessary but SLSA L3's bar is stronger than that). **For
   this ADR: L2 is achievable and is the right target; claiming L3 would overstate what a
   standard GHA-hosted two-job split proves.**
6. **crates.io index integrity is transport/storage-only, with no authorship signature** — the
   sparse index (default since Rust 1.70) serves index entries over HTTPS with a `cksum`
   (SHA-256 of the `.crate` file) per version
   ([Registry Index docs](https://doc.rust-lang.org/cargo/reference/registry-index.html)). This
   matches `research_mirror_supply_chain.md`'s finding that crates.io's sparse index is "the
   negative example" in the TUF taxonomy — "tolerable only because crates.io is centrally
   operated; not a model to copy." Nothing has changed this in 2025–2026: no TUF/in-toto layer
   sits under the crates.io index today. `--locked` + a mandatory-since-1.84 `Cargo.lock` in
   every `.crate` ([cargo#14815](https://github.com/rust-lang/cargo/pull/14815)) is the
   strongest artifact-side pinning available, and it pins transitive dependency *versions*, not
   their authorship.
7. **Reproducibility is a set of opt-in knobs, not a property `cargo install` gets by default**
   — `SOURCE_DATE_EPOCH`, `--remap-path-prefix`/the newer `--trim-paths`
   ([RFC 3127](https://rust-lang.github.io/rfcs/3127-trim-paths.html)) address embedded
   timestamps and absolute build paths, but neither is on by default for `cargo install`, and
   neither controls a third-party crate's own `build.rs` (which can embed non-deterministic
   values via `env!`, wall-clock reads, or unordered codegen) or the six-native-target build
   matrix's inherent non-determinism sources (rustc codegen unit scheduling, linker version
   skew across runner images). Nixpkgs/Debian achieve reproducibility only by controlling the
   *entire* toolchain and sandbox, which a mirror re-executing arbitrary upstream crates on
   GitHub-hosted runners does not do. **The honest contract for this ADR is "immutable and
   provenance-attested once pushed," not "byte-reproducible on demand."**
8. **`cargo-binstall`/`cargo-quickinstall` — the closest existing "rebuild and redistribute
   Rust binaries" precedent — offer only optional, maintainer-opt-in signature verification**,
   not a default trust guarantee: Binstall supports a maintainer-specified signing public key
   and `--only-signed` to refuse unsigned packages, but this is off unless the crate maintainer
   configures it ([cargo-binstall#1](https://github.com/cargo-bins/cargo-binstall/issues/1)).
   quickinstall's own telemetry (crate name, version, target triple) is the only thing
   consumers get by default beyond a prebuilt binary — there is no provenance predicate today.
   **This means ocx-mirror's SLSA-provenance-on-every-rebuilt-artifact plan would be a
   meaningfully stronger guarantee than the closest existing prior art in this space**, not
   parity work.
9. **Crate-name and version-string grammar are simple enough to validate with an allowlist
   regex before ever reaching a subprocess boundary**: crates.io restricts crate names to
   ASCII alphanumerics, `-`, and `_`, max 64 characters, non-empty, excluding reserved/Windows
   device names (`nul`, etc.) — no shell metacharacters, no path separators, no leading dash
   (which would be parsed as a flag) are valid by the registry's own rules. Cargo's version
   requirement grammar (`^`, `~`, `*`, comma-separated, exact) is fully re-implementable via
   the `semver` crate's `VersionReq::parse`, which "follows the implementation choices made by
   Cargo" ([dtolnay/semver](https://github.com/dtolnay/semver)) — i.e. the mirror does not need
   to hand-roll a parser; it can reject anything `semver::VersionReq::parse` rejects before
   ever building a `cargo install <name>@<version>` argv.

## Recommendation

**Mandatory** (attacker: upstream crate author / compromised transitive dependency, exercised
via arbitrary `build.rs`/proc-macro code on the build leg):

- **Two-job split, implemented as a reusable-workflow boundary, not a composite action.** Build
  jobs (per native target) carry zero registry/push credentials, zero write access to
  `packaging/metadata.json` or any signing key; only the `push` job — a separate reusable
  workflow job the build jobs cannot execute inside of — holds the registry credential. This is
  the control the owner ruling names, and it is the one control the whole ecosystem converges
  on for exactly this threat (Finding 5).
- **Re-verify artifact integrity at the build→push hand-off**, not by trusting
  `actions/upload-artifact` alone: each build job emits a checksum (as a job output / step
  summary, signed if feasible) alongside the binary; the push job recomputes and compares
  before it ever signs or pushes. Finding 4 shows the artifact store alone is not a tamper
  barrier between jobs in the same workflow.
- **Validate crate name and version string against the registry's own grammar before spawning
  `cargo install`**: name — ASCII `[A-Za-z0-9_-]`, ≤64 chars, non-empty, reject reserved/device
  names; version — must parse under `semver::VersionReq` semantics. Reject anything else before
  it reaches a subprocess argv. Attacker: upstream index content (a catalog entry containing a
  crafted name/version string is exactly the "unvalidated external input at a subprocess
  boundary" class the threat model calls Block-tier regardless of this ADR).
- **Attach SLSA v1 provenance (in-toto `buildDefinition`/`runDetails`) to every rebuilt
  artifact**, stating source crate + exact version + `Cargo.lock` cksum + rustc version +
  feature flags + target triple + builder identity (the GitHub-hosted runner/workflow), signed
  through the same keyless Fulcio/Rekor path the mirror already uses for OCI copies
  (`adr_mirror_signing.md`). This is a stronger provenance guarantee than either
  `cargo-binstall`/`cargo-quickinstall` offer today (Finding 8) and is the standard, current
  shape (SLSA Build L2) for "we built this, here's proof of how" without claiming
  reproducibility.
- **`--locked` against the crate's shipped `Cargo.lock`** (mandatory in every `.crate` since
  Cargo 1.84): pins the full transitive dependency set that will execute during the build to
  exactly what the crate author resolved against, closing the "silently pull a newer,
  compromised transitive dependency at install time" gap. Attacker: a transitive dependency
  compromised *after* the target crate's release.
- **Do not claim byte-reproducibility.** State the contract as "immutable once pushed" per
  Finding 7 and per this project's existing signing model — promising reproducibility the
  ecosystem cannot deliver for arbitrary third-party crates would be a false verification claim
  under `quality-core.md`.

**Optional** (defense-in-depth / operator choice, not required for the ADR to close):

- **OIDC-minted push credential** (crates.io Trusted Publishing pattern) instead of a stored
  registry PAT for the `push` job, if the target registry supports GitHub OIDC federation.
  Reduces standing-secret exposure but is orthogonal to the build-leg threat this ADR is
  scoping — it protects the *push* credential, which the two-job split has already kept off
  the build leg entirely.
- **`cargo audit` / `cargo deny` as a pre-build advisory gate** on the resolved `Cargo.lock`,
  surfacing known-malicious/yanked crates before `cargo install` runs. Attacker: a crate later
  flagged in the RustSec advisory database after initial publication — this is a
  freshness/detection control, not a containment control, so it's a nice-to-have layered on
  top of, not instead of, the two-job split.
- **Egress-restricted build runners** (e.g., an allowlist limited to `crates.io`/`static.crates.io`
  during the build step) as a stronger-than-ecosystem-default mitigation against a `build.rs`
  exfiltration/second-stage-download attempt (Findings 1–2). Not mandatory because the owner
  ruling already accepts arbitrary build-leg code execution as in-scope-and-accepted on a
  secretless runner — egress restriction narrows the *blast radius of what that code can do*
  (can't call home) even though it has nothing to steal; worth flagging as a cheap upgrade, not
  a blocker.
- **cargo-vet-style pre-admission human review** for a curated allowlist of crates, if the
  mirror wants a "reviewed" tier above "any crates.io crate." Meaningful ecosystem momentum
  (Finding, Trending table) but a policy/curation feature, not a security requirement for the
  base `source.type: cargo` design.

## Sources

| Source | Type | Date | Relevance |
|---|---|---|---|
| [security-threat-model.md](../rules/security-threat-model.md) | Internal rule | 2026-08-14 | Defines the attacker boundary this whole research is scoped by |
| [research_mirror_supply_chain.md](./research_mirror_supply_chain.md) | Internal artifact | 2026-08-13 | TUF taxonomy; crates.io sparse index named as "the negative example" — reused in Finding 6 |
| [adr_mirror_signing.md](./adr_mirror_signing.md) (Decisions header only) | Internal artifact | — | Existing keyless Fulcio/Rekor signing path this ADR's provenance recommendation reuses |
| [Sandbox build.rs and proc macros — Rust Internals](https://internals.rust-lang.org/t/sandbox-build-rs-and-proc-macros/16345) | Forum/primary discussion | ongoing thread | build.rs/proc-macro unsandboxed-by-design status |
| [RUSTSEC-2022-0042: rustdecimal](https://rustsec.org/advisories/RUSTSEC-2022-0042.html) | Advisory (primary) | 2022-05 | Named incident, CI-secret-targeting build-time payload |
| [Official rustdecimal advisory — blog.rust-lang.org](https://blog.rust-lang.org/2022/05/10/malicious-crate-rustdecimal/) | Primary/official | 2022-05-10 | Corroborates RUSTSEC-2022-0042 detail |
| [SentinelOne — CrateDepression](https://www.sentinelone.com/labs/cratedepression-rust-supply-chain-attack-infects-cloud-ci-pipelines-with-go-malware/) | Security vendor research | 2022-05 | Named incident, CI pipeline targeting |
| [Socket.dev — 5 malicious Rust crates, .env exfiltration](https://socket.dev/blog/5-malicious-rust-crates-posed-as-time-utilities-to-exfiltrate-env-files) | Security vendor blog (secondary aggregation) | 2026 (exact date unconfirmed) | 2026 campaign; **flagged — secondary source, could not cross-verify against a primary RUSTSEC ID in the time budget** |
| [RFC 3691 — Trusted Publishing on crates.io](https://rust-lang.github.io/rfcs/3691-trusted-publishing-cratesio.html) | Official RFC | accepted 2025 | OIDC mechanism, claims checked, stated limitations |
| [crates.io development update, 2026-01-21](https://blog.rust-lang.org/2026/01/21/crates-io-development-update/) | Official blog | 2026-01-21 | Trusted Publishing GA + GitLab CI extension |
| [SLSA Build Levels v1.2](https://slsa.dev/spec/v1.2/levels) | Official spec | current (v1.2) | L1/L2/L3 definitions used throughout Findings 5, Recommendation |
| [SLSA Provenance v1.2](https://slsa.dev/spec/v1.2/build-provenance) | Official spec | current (v1.2) | in-toto predicate shape recommended for rebuilt-artifact provenance |
| [slsa-github-generator BYOB.md](https://github.com/slsa-framework/slsa-github-generator/blob/main/BYOB.md) | Official repo docs | actively maintained | Reusable-workflow vs. composite-action isolation distinction (Design Patterns) |
| [Imre Rad — "GitHub artifact immutability is a lie"](https://irsl.medium.com/github-artifact-immutability-is-a-lie-9b6244095694) | Independent security research | — (unconfirmed date, **flag: verify recency**) | Cross-job artifact tamper finding, directly informs Recommendation's hand-off control |
| [Registry Index — The Cargo Book](https://doc.rust-lang.org/cargo/reference/registry-index.html) | Official docs | current | Sparse index `cksum` field, transport/storage integrity only |
| [cargo#14815 — always include Cargo.lock](https://github.com/rust-lang/cargo/pull/14815) | Official repo (merged PR) | landed for Cargo 1.84 (Dec 2024) — **>18 months old, re-verify still current default before ADR ships** | `Cargo.lock` mandatory-in-package change |
| [dtolnay/semver](https://github.com/dtolnay/semver) | Official/canonical crate | actively maintained | Version-requirement grammar for input validation |
| [Specifying Dependencies — Cargo Book](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html) | Official docs | current | Version requirement syntax detail |
| [reproducible-builds.org — Rust](https://reproducible-builds.org/docs/rust/) | Independent project docs | **not dated in fetch — flag: verify recency before citing precise claims** | `SOURCE_DATE_EPOCH`/remap-path-prefix knobs |
| [RFC 3127 — trim-paths](https://rust-lang.github.io/rfcs/3127-trim-paths.html) | Official RFC | merged | `--trim-paths` as successor to ad hoc `--remap-path-prefix` usage |
| [cargo-binstall#1 — signing support](https://github.com/cargo-bins/cargo-binstall/issues/1) | Official repo issue | open/ongoing | Baseline for "what consumers of a rebuild-and-redistribute tool get today" |
| [Nix build sandbox — ayats.org](https://ayats.org/blog/nix-tuto-2) | Independent tutorial | **undated in fetch — flag** | Network-isolated build sandbox model, contrasted with cargo's lack of one |
| [Fixed-output derivations — phip1611.de](https://phip1611.de/blog/accessing-network-from-a-nix-derivation/) | Independent blog | **undated in fetch — flag** | Nix's narrow-hole-for-pinned-output pattern |

## Gaps / not fully resolved within the 25-minute budget

- Did not independently verify the 2026 `.env`-exfiltration campaign (`chrono_anchor` et al.)
  against a primary RUSTSEC advisory ID or crates.io's own incident postmortem — cited via
  Socket.dev secondary reporting only; **re-verify before using as the sole incident evidence
  in the ADR** (rustdecimal/CrateDepression are primary-sourced and sufficient on their own).
- Did not fetch docs.rs's own published sandbox/network-policy page directly (time-boxed to a
  search-result characterization) — if the ADR needs an exact docs.rs egress policy citation,
  fetch `docs.rs` build documentation directly.
- Conda-forge's specific build-isolation policy (as distinct from Nix's, which was covered) was
  not found in the time budget — the search returned only Nix/nixpkgs results.
