# ocx 0.6.0 — semantic (non-compile, non-CLI-spelling) impact on ocx-mirror

> **CORRECTION — read this before acting on anything below (2026-08-31).**
> This record was produced by a read-only analysis that could not execute
> anything, and three of its claims did not survive verification against the
> real binaries. Do **not** re-attempt them:
>
> - **Item 1 (plain HTTP / `insecure_hosts`) — REFUTED.** `insecure_hosts` reads
>   only its two arguments; `system_locked_shut` (`config/insecure.rs:135`) scans
>   `config.registries` and touches no filesystem, no `$OCX_HOME`, no managed
>   scope. `insecure_hosts(&Config::default(), env)` is the env list minus
>   duplicates, and dedup is inert for both membership-test consumers. The
>   mirror's blindness to config-declared hosts is the pre-existing
>   `Config::default()` choice, not this call site. No change needed.
> - **Item 3 (any-pin provenance) — REFUTED.** `verify_any_pin_provenance` fires
>   only when the push target is `platform.is_any()`; every env leg routes through
>   `pylock_target_platform`, which rejects anything but `Platform::Specific`
>   before `select_interpreter_pin`. The mirror cannot emit an any-targeted push.
>   The record also misreads `prepare.rs:570` — "rejected by the publish gate"
>   there is about *index-digest* pins, not any-pin provenance.
> - **Item 9a (reserved tags) — OVERSTATED.** No red is achievable:
>   `registry_tag_newer_than` gates on `key.0.is_some()` and every reserved shape
>   parses to `None` under `pep440_sort_key`. The filter swap to
>   `Tag::is_reserved_str` landed as anti-drift hardening, not a bug fix. And the
>   mirror **deliberately keeps writing `sha256.<hex>`**: ocx documents
>   `Tag::LegacyKeep` as a permanent read arm (`tag.rs:124-131`), `is_reserved()`
>   is true for both spellings, and there are zero read-side consumers of the
>   spelling — switching would add a second tag to every manifest on every
>   already-mirrored destination with nothing deleting the old one.
>
> Items 2, 4, 5, 6, 7 and 9b-d stand as written. Verification detail:
> `notes_wp6_tagscheme.md`. Deferred items became
> [#59](https://github.com/ocx-sh/ocx-mirror/issues/59) and
> [#60](https://github.com/ocx-sh/ocx-mirror/issues/60).


Read-only analysis. Branch `feat/ocx-0.6-adoption`, submodule `external/ocx` at
`v0.6.0` (`e48ef73c`). Sibling agents own the compile breakage (WP1) and the
CLI verb renames (WP2/WP3). This record covers only **behaviour that compiles
and passes the current tests but changes what the mirror does at runtime**.

Evidence paths: bare paths are `/home/mherwig/dev/ocx-mirror/…`; paths under
`external/ocx/` are the vendored ocx tree at v0.6.0.

---

## Question

For each 0.6.0 change, does ocx-mirror's runtime behaviour change, and where?

---

## Verdict table

| # | Item | Verdict | Action |
|---|------|---------|--------|
| 1 | Plain HTTP per registry, exit 65 | **AFFECTED** | `registry_client()` must resolve the config∪env union, not the raw env list |
| 2 | Configured index authoritative for its registry | **NOT AFFECTED** | none — mirror builds no `ChainedIndex` |
| 3 | Any-pin provenance on the canonical registry | **AFFECTED** | python-env push under `OCX_MIRRORS` now reads canonical; fails closed if unreachable |
| 4 | Cascade into a not-yet-existing repo (`--new` removed) | **AFFECTED — strict improvement** | drop `--new`; no new first-publish failure mode |
| 5 | Hard links resolve under the extraction root | **AFFECTED — mostly improvement** | in-tree hardlinks now work; absolute-target tarballs now hard-fail |
| 6 | Wasm platforms + enforced platform pairs | **NOT AFFECTED** | strictly additive; verified empirically |
| 7 | Shell reconcile replacing direnv | **NOT AFFECTED (code)** | `ocx direnv` still ships; no mirror code references it |
| 8 | Cached auth handshake / fast `index sync` | **AFFECTED — advisory** | mirror's 4 subprocess attempts × ocx's 3 HTTP attempts = 12 |
| 9a | `canonical_tags` → `keep_tags`, tag shape `sha256.<hex>` → `__ocx.keep.<alg>-<hex>` | **AFFECTED — highest impact** | two mirror sites now wrong; see below |
| 9b | New exit codes 83/84/85 | **NOT AFFECTED** | additive; retry ladder keys only on 75 |
| 9c | `ClientError::Mirrored` wrapper | **NOT AFFECTED** | delegates `classify()` to the wrapped error |
| 9d | Wire digest mismatch → data error | **NOT AFFECTED** | `DigestMismatch` was already exit 65 in 0.5.8 |
| 9e | New `OCX_*` env keys not forwarded to subprocess | **UNCERTAIN — owner call** | 4 signing vars absent from the forward list |
| — | Dependency feature-list drift | **NONE outstanding** | `url` doc claim in CLAUDE.md is stale |

---

## Evidence and per-item verdict

### 1. Declare plain HTTP per registry; refuse unsafe destinations with exit 65 — **AFFECTED**

`OCX_INSECURE_REGISTRIES` still exists and is still honoured — `env.rs:1644`
is byte-identical between v0.5.8 and v0.6.0, and empirically ocx 0.6.0's own
error names both routes:

```
could not connect to 'nonexistent.invalid' over https; if it serves plain HTTP,
set insecure = true under [registries."nonexistent.invalid"] or add the host to
OCX_INSECURE_REGISTRIES
```

What is new is a **second declaration source** and a **subtraction**.
`external/ocx/crates/ocx_lib/src/config/insecure.rs:96` (new file, 478 lines):

```rust
pub fn insecure_hosts(config: &Config, env: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = config
        .registries
        .iter()
        .flatten()
        .filter(|(name, registry)| registry.insecure.unwrap_or(false) && !system_locked_shut(config, name))
        .map(|(name, _)| name.clone())
        .collect();

    for host in env {
        if system_locked_shut(config, host) || hosts.contains(host) { continue; }
        hosts.push(host.clone());
    }
    hosts
}
```

Module doc, same file, lines 4–17: *"Two sources declare it — an `insecure = true`
entry under `[registries."<name>"]`, and the `OCX_INSECURE_REGISTRIES` env list —
and they are a **union** … There is exactly one subtraction, and only the system
scope can make it."*

The mirror bypasses both. `src/command/package/mod.rs:84`:

```rust
pub(crate) fn registry_client() -> Result<ocx_lib::oci::Client, MirrorError> {
    let insecure = ocx_lib::env::insecure_registries();
    let env_mirrors = ocx_lib::env::mirrors()…?;
    let resolved = ocx_lib::resolve_mirror_map(&ocx_lib::Config::default(), env_mirrors, &insecure)…?;
    Ok(ocx_lib::oci::ClientBuilder::new()
        .plain_http_registries(insecure)
        .mirrors(ocx_lib::oci::MirrorMap::new(resolved.registry))
        .build())
}
```

`insecure` here is the **raw env list**. Two consequences, opposite directions:

- **Fails closed where ocx succeeds.** An operator who declares
  `[registries."localhost:5001"] insecure = true` — the newly documented way —
  gets it honoured by every `ocx` subprocess leg and **ignored** by the mirror's
  own in-process client. `registry: localhost:5001` is a real configured target
  (`src/pipeline/registry_sync/tests.rs:92`, `src/spec/registry/tests/support.rs:28`).
- **Fails open where ocx refuses.** A system-scope `/etc/ocx/config.toml` entry
  stating `insecure = false` revokes the env grant for `ocx`, but the mirror still
  goes plaintext (CWE-319 direction).

`resolve_mirror_map`'s third parameter is literally named `insecure_hosts`
(`external/ocx/crates/ocx_lib/src/config/mirror.rs:531-534`, *"an `http://`
mirror whose host is not in `insecure_hosts` is a hard error"*) — it wants the
resolved union, not the env list.

`ClientBuilder::plain_http_registries` itself is unchanged
(`external/ocx/crates/ocx_lib/src/oci/client/builder.rs:120-126`), so this is a
call-site defect, not an API break.

Subprocess legs are covered: `OCX_INSECURE_REGISTRIES` is in the forward list at
`src/pipeline/ocx_cli.rs:46`.

**Action:** replace both `insecure` uses with
`ocx_lib::insecure_hosts(&config, &ocx_lib::env::insecure_registries())` over a
*loaded* config, not `Config::default()`. Passing `Config::default()` also means
config-declared mirrors are ignored — pre-existing, same fix.

### 2. Make a configured index authoritative for its whole registry — **NOT AFFECTED**

The change is in `ChainedIndex`. `external/ocx/crates/ocx_lib/src/oci/index/chained_index.rs:833`:

```rust
if authoritative {
    if let Some(base_url) = source.index_base_url() {
        return Err(super::error::Error::NotInIndex {
            identifier: identifier.to_string(),
            namespace: identifier.registry().to_string(),
            base_url: base_url.to_string(),
        }.into());
    }
    log::debug!("Authoritative source has no '{}' — stopping.", identifier);
    return Ok(None);
}
```

Previously the `Ok(None)` fell through to a direct registry pull; now an
authoritative source with a base URL hard-errors.

The mirror never constructs a `ChainedIndex`. Its only index construction is
`src/command/package/pipeline/prepare.rs:580`:

```rust
let index = ocx_lib::oci::index::Index::from_remote(ocx_lib::oci::index::OciIndex::new(
    ocx_lib::oci::index::OciIndexConfig { client: client.clone() },
));
```

`Index::from_remote` boxes the `OciIndex` directly with no chain
(`external/ocx/crates/ocx_lib/src/oci/index.rs:183-187`), and `index_base_url()`
is overridden only by `OcxIndex`
(`external/ocx/crates/ocx_lib/src/oci/index/ocx_index.rs:1519`); `OciIndex`
inherits the `None` default at `index_impl.rs:202`. Both guard conditions are
therefore unreachable from the mirror. Its other index imports
(`CatalogIndex`, `OciIndex`, `IndexRoot`, `RootTag`, `serialize_root`,
`parse_physical_repository` — `src/pipeline/registry_sync*.rs`) are wire types
and the registry-tags-derived index, not the chain.

`RootScope::Tag(tag)` → `RootScope::Tags(&[tag])` (`chained_index.rs:798`) is
internal to `commit_published_root`, which the mirror does not call.

**Caveat, not a finding:** a *subprocess* `ocx` leg does build the default chain.
If an operator declares `index = …` for the push target's namespace, a pull
between `ocx package push` and the index sync now returns `NotInIndex` instead of
falling through to the registry. The mirror performs no post-push pull, so
nothing in-tree hits this — but the acceptance harness and the contrib fleet
should be spot-checked by WP3.

### 3. Decide any-pin provenance on the canonical registry — **AFFECTED**

An "any-pin" is a `Dependency` whose pinned leaf is advertised as
`Platform::Any`. `external/ocx/crates/ocx_lib/src/publisher/publish_gate.rs:134`:

```rust
// Canonical, never a mirror: this read gates a publish, and Invariant #5
// says a read that decides a write names the same host the write lands on.
let (digest, manifest) = client
    .fetch_manifest_addressed(dependency_identifier, ReadAddressing::Canonical)
    .await
    .map_err(|source| PublishGateError::AnyPinProvenanceUnavailable { … })?;
```

was `client.fetch_manifest(dependency_identifier)` in v0.5.8 — which at that time
was mirror-aware.

**The mirror does construct pins.** `src/command/package/pipeline/prepare.rs:609-634`
(`select_interpreter_pin`) attaches a Python interpreter as the composed
package's `PRIVATE` dependency, and `crates/ocx_python/src/compose.rs:346`
folds it into `Dependencies`. The pin is a `select_best(platform, candidates)`
winner, so for a leg whose platform is `Platform::Any` the winner is an `any`
leaf — an any-pin. The mirror's own doc comment above `fetch_interpreter_candidates`
says as much: *"is rejected by the publish gate at push time"*.

The gate runs inside the subprocess the mirror shells out to —
`external/ocx/crates/ocx_cli/src/command/package_push.rs:309`:
`publisher::verify_dependency_pins(publisher.client(), &valid, &platform).await?`.

**Wrong behaviour:** a mirror run with `OCX_MIRRORS` configured now sends the
provenance read to the canonical registry. In a mirror-only or restricted-egress
environment the canonical host is unreachable, and the gate is fail-closed —
`AnyPinProvenanceUnavailable` aborts the push where 0.5.8 satisfied it from the
mirror. The change is correct (it closes a forged-provenance path, pinned by
`publish_gate.rs:353-407`), but it is a new hard dependency on canonical
reachability for the python-env push leg specifically.

**Action:** confirm the python-mirror deployment can reach the canonical
registry directly, or accept that any-pin legs require it.

### 4. Cascade into a target repository that does not exist yet — **AFFECTED, strict improvement**

Confirmed. `external/ocx/crates/ocx_lib/src/oci/client.rs:477`:

```rust
/// [`list_tags`](Self::list_tags), with an absent repository answered as
/// the empty list it is.
///
/// For a cascade prelude only: "which rolling tags may move" has an
/// authoritative answer for a repository that has never been published to,
/// and it is "none of them are taken". Narrow on purpose — every other
/// failure still propagates, so a transient 5xx can never be mistaken for
/// an empty tag list and cascade against a listing nobody read (#157).
pub(crate) async fn list_tags_or_empty_addressed(
    &self,
    identifier: Identifier,
    addressing: ReadAddressing,
) -> Result<Vec<String>> {
    match self.list_tags_addressed(identifier, addressing).await {
        Err(crate::Error::OciClient(ClientError::RepositoryNotFound(_))) => Ok(Vec::new()),
        other => other,
    }
}
```

Exactly one arm folds. `RepositoryNotFound` is produced only by the 404 /
`NAME_UNKNOWN` mapper at
`external/ocx/crates/ocx_lib/src/oci/client/native_transport.rs:305-328`. Auth
(→ `AuthError`, 80) and 5xx (→ `RegistryTransient`, 75) both propagate.

`Publisher::list_tags` (`publisher.rs:317-323`) now routes through it with
`ReadAddressing::Canonical`, documented as: *"deciding that from a mirror is the
Invariant #5 / CWE-345 fail-open the copy path already fixed, and a stale mirror
missing a repository the canonical registry does publish would silently move
`latest` backwards."*

**Verdict: strict improvement, no new first-publish failure mode.** The mirror's
unconditional `--new` made a cascade tolerate *any* tag-listing failure by
cascading against an empty list — a transient 5xx or an expired token could
re-point `latest`/`X`/`X.Y` backwards. 0.6.0 narrows the fold to exactly the case
`--new` was meant for (a brand-new repository) and aborts on everything else.
Dropping the flag loses nothing and closes the fail-open.

### 5. Hard links resolve under the extraction root — **AFFECTED, mostly improvement**

The mirror uses ocx's extractor: `src/pipeline/package.rs:7`
(`use ocx_lib::archive::{Archive, ExtractOptions};`) and `:485`, plus
`src/pipeline/orchestrator/tests/bin_scan.rs:342,398`.

`external/ocx/crates/ocx_lib/src/archive/tar.rs` is +123/-0 — the whole
`EntryType::Link` arm is new. Lines 252-260:

```rust
/// `tar::Entry::unpack` cannot be trusted with hard links: it calls
/// … hands the archive's raw link name to `fs::hard_link` verbatim (tar 0.4.46,
```

and 215-219:

```rust
resolve_hard_link_source(output, &target, strip_components).ok_or_else(|| Error::HardLinkEscape {
…
std::fs::hard_link(&source, &output_path).map_err(|e| Error::Io { …
```

Two behaviour changes for an upstream tool tarball:

- **Legitimate in-tree hardlinks now extract.** Test
  `legitimate_in_tree_hard_link_extracts` (`tar.rs:341`) covers `dir/original.txt`;
  its doc says the raw name previously *"resolved it against the process CWD"*.
  A multi-call-binary tarball (busybox-style, `bin/foo` hardlinked to `bin/bar`)
  that failed or mis-linked under 0.5.8 now works.
- **Absolute or escaping hardlink targets now hard-fail** with
  `Error::HardLinkEscape` (`external/ocx/crates/ocx_lib/src/archive/error.rs:33`)
  rather than silently linking a host file into the tree. Correct, but a tarball
  that "worked" before can now fail a mirror run.

**Action:** none required. If a contrib mirror starts failing on
`hard link '…' target '…' does not resolve inside the extraction root`, that is
the fix working, not a regression.

### 6. Wasm platforms and enforced platform pairs — **NOT AFFECTED**

`external/ocx/crates/ocx_lib/src/oci/platform.rs:35-44` is the new gate:

```rust
pub const SUPPORTED_PAIRS: &[(OperatingSystem, Architecture)] = &[
    (OperatingSystem::Linux,   Architecture::Amd64),
    (OperatingSystem::Linux,   Architecture::Arm64),
    (OperatingSystem::Darwin,  Architecture::Amd64),
    (OperatingSystem::Darwin,  Architecture::Arm64),
    (OperatingSystem::Windows, Architecture::Amd64),
    (OperatingSystem::Windows, Architecture::Arm64),
    (OperatingSystem::Wasip1,  Architecture::Wasm),
    (OperatingSystem::Wasip2,  Architecture::Wasm),
];
```

enforced by `validate_pair` at both choke points (`FromStr` at `:610`,
`TryFrom<native::Platform>` at `:811`).

**The enforcement cannot reject anything the mirror could previously emit.**
In v0.5.8 `OperatingSystem` was `{Darwin, Linux, Windows}`
(`git show v0.5.8:…/operating_system.rs:33-35,55`) and `Architecture` was
`{Amd64, Arm64}` (`…/architecture.rs:33-34`), and there was **no** `SUPPORTED_PAIRS`
or `validate_pair` at all (`git show v0.5.8:…/platform.rs` — zero matches). The
v0.5.8 parseable set is therefore exactly the 6 native pairs, all 8 of which are
in `SUPPORTED_PAIRS`. The only new rejections combine an old OS with the new
`Wasm` arch, or a new `wasip*` OS with an old arch — combinations that did not
parse in v0.5.8 either. The change is strictly additive.

Empirically confirmed against the installed ocx 0.6.0 binary
(`ocx package pull --platform <p> nonexistent.invalid/x:1`):

| platform | result |
|---|---|
| `linux/amd64`, `darwin/arm64`, `windows/arm64`, `wasip1/wasm`, `any` | accepted (reaches DNS failure) |
| `linux/wasm` | `error: invalid value 'linux/wasm' … unsupported platform` |
| `wasip1/amd64` | `error: invalid value 'wasip1/amd64' … unsupported platform` |

The refusal even enumerates the allowed set:
`linux/amd64, linux/arm64, darwin/amd64, darwin/arm64, windows/amd64, windows/arm64, wasip1/wasm, wasip2/wasm`.

Mirror-side mapping at `src/command/package/pipeline/plan/env.rs:490-524`
destructures `Platform::Specific { os, arch, .. }` and now carries explicit
`Wasip1 | Wasip2` and `Wasm` arms that error (`ocx_python`'s `TargetOperatingSystem`
has no wasm) — already handled by WP1.

**Action:** none. The `Display`/`FromStr` grammar itself is unchanged, so
platform strings used as map keys and path segments round-trip identically.

### 7. Reconcile the environment on every prompt, replacing direnv — **NOT AFFECTED (code)**

`ocx direnv` still ships in 0.6.0 — verified against the binary:

```
Usage: ocx direnv [COMMAND]
Commands:
  init    Write a `.envrc` wiring `ocx direnv export` into direnv
  export  Print stateless shell exports for the project toolchain (direnv entry point)
```

The shell reconcile is an addition (`ocx shell`, `crates/ocx_lib/src/shell/reconcile*`),
not a removal. No mirror Rust source references direnv; the only near-miss is a
comment at `src/command/package/pipeline/generate/templates/workflow.yml:208-211`
about deliberately *not* using `ocx exec --`. `.envrc` and docs are WP3's.

### 8. Cached auth handshake, fast `index sync` — **AFFECTED (advisory only)**

The two layers do not conflict; they **multiply**.

- ocx 0.6.0 request-level: `RetryPolicy` with `max_attempts: 3`
  (`external/ocx/crates/ocx_lib/src/oci/transport_policy.rs:143,163`), plus a new
  `RetryBudget` (`:282-322`) and `TransportHardening` (`:338`).
- mirror subprocess-level: `src/pipeline/ocx_cli/push.rs` retries the whole
  `ocx package push` up to `max_retries: 3` (four attempts), each bounded by
  `PUSH_TIMEOUT = 3600s`, keyed only on exit 75
  (`push_exit_is_transient`, `:151`).

Worst case per tile is now 4 × 3 = **12 registry attempt sequences**, up to four
hours. The mirror's own comment already flags the wall-clock risk (*"two tiles
doing it do not [fit]"*). Auth-handshake coalescing reduces round-trips within a
single `ocx` process and does not interact with the subprocess ladder.

**Action:** advisory. If push wall-clock regresses on the fleet, the lever is
`max_retries`, not `PUSH_TIMEOUT`.

### 9a. `canonical_tags` → `keep_tags` and the tag-shape change — **AFFECTED, highest impact**

`external/ocx/crates/ocx_lib/src/publisher.rs:60-66` — the field renamed and the
**tag shape changed**:

```rust
/// Digest-named `__ocx.keep.<algorithm>-<hex>` tags written by this push, in push order,
```

`external/ocx/crates/ocx_lib/src/oci/client.rs:752`:

```rust
let tag = format!("{}{algorithm}-{hex}", InternalTag::KEEP_TAG_PREFIX);
```

with `pub const KEEP_TAG_PREFIX: &str = "__ocx.keep.";`
(`external/ocx/crates/ocx_lib/src/package/tag.rs:62`). ocx ships the filter the
mirror needs: `Tag::is_reserved_str(&str)` (`tag.rs:298`), a documented
*"convenience wrapper over `Tag::is_reserved` for listing filters"* that already
covers the `__ocx` namespace, the **frozen legacy `sha256.<hex>` form**
(`Tag::LegacyKeep`, classification step 4 at `tag.rs:~326`), and cosign /
OCI-Referrers fallback sidecar tags. The CLI flag moved
`--no-canonical-tag` → `--no-keep-tag`. This is a **hard switch**, not a dual
write: `push_keep_tag` writes only the new form, so a repository now holds
`sha256.<hex>` tags from historical pushes and `__ocx.keep.sha256-<hex>` from new
ones.

Two mirror sites are now wrong, and **neither is a compile error**:

1. **`src/command/package/pipeline/push/alias.rs:158`** — the rolling-alias
   ordering filter:

   ```rust
   .filter(|tag| *tag != "latest" && !tag.starts_with("sha256."))
   ```

   A `__ocx.keep.sha256-<hex>` tag does not start with `sha256.`, so it survives
   the filter and is fed to `pep440_sort_key`. Its doc comment
   (`alias.rs:149-153`) states the intent: *"Rolling and canonical tags are not
   versions and are skipped."* An unparseable key yields
   `key.0.is_some() == false`, so it cannot be *selected* as "newer" — the
   immediate blast radius is bounded — but the filter no longer expresses the
   invariant it documents, and the test that pins it
   (`src/command/package/pipeline/push/tests/ordering.rs:9`) still uses the old
   `"sha256.abc123"` fixture, so it passes while covering nothing real.

   **Action:** replace the hand-rolled predicate with
   `ocx_lib::package::tag::Tag::is_reserved_str(tag)`. It covers both the new
   `__ocx.keep.*` form and the historical `sha256.<hex>` form in one call, and
   additionally excludes cosign `.sig`/`.att`/`.sbom` sidecar tags — which
   0.6.0's copy path now carries to destinations, and which the current filter
   never handled. Add a `__ocx.keep.sha256-…` row to the ordering fixture.

2. **`src/pipeline/registry_copy.rs:999-1018`** — the mirror writes its **own**
   digest tag at the copy destination:

   ```rust
   /// Tag one copied manifest with its own digest — `sha256.<hex>`, the form
   …
   async fn push_canonical_tag(
   ```

   gated by `context.canonical_tags` (`:993`), from the `canonical_tags:` spec key
   (`src/spec/registry.rs:114-133`, default `true`). This is mirror-owned code, so
   it keeps emitting `sha256.<hex>` while `ocx package push` emits
   `__ocx.keep.sha256-<hex>` — the two now disagree about the deletion-safety-net
   tag shape at the same registry.

   **Action:** owner decision. Either follow ocx to `__ocx.keep.<alg>-<hex>` (and
   rename the spec key), or document the divergence deliberately. Affected
   acceptance tests: `test/tests/test_registry_sync.py:624-628,664-665`,
   `test/tests/test_mirror_patch.py:376`. Also
   `src/pipeline/registry_sync.rs:364`, `src/pipeline/python_push.rs:42-43`,
   `src/command/package/pipeline/announce.rs:397` (`reserved_tags_dropped` fixture
   still uses `format!("sha256.{}", …)`).

Compile-side (WP1's, noted for completeness): `PushOutcome` is now
`#[non_exhaustive]` with a `PushOutcome::new` constructor and a new
`platform_digests` field, and the doc says so explicitly —
*"ocx-mirror takes `ocx_lib` as a path dependency, so a later field would break
it at a struct literal"* (`publisher.rs:41-47`).

### 9b–9d. Remaining Fixed-section items — **NOT AFFECTED**

- **New exit codes.** `external/ocx/crates/ocx_lib/src/cli/exit_code.rs` adds
  `TransparencyLogUnavailable = 83`, `ReferrersUnsupported = 84`,
  `UnsupportedKeyBackend = 85`. 0–82 are unchanged. The mirror's
  `push_exit_is_transient` matches only `TempFail` (75), so the new codes are
  correctly terminal.
- **`ClientError::Mirrored`.** New wrapper carrying `{origin, mirror, physical}`
  (`external/ocx/crates/ocx_lib/src/oci/client/error.rs`) whose `classify()` is
  `Self::Mirrored { source, .. } => return source.classify()`. Exit codes survive
  a mirror hop unchanged; only the message gains provenance.
- **"Classify a wire digest mismatch as a data error."** `DigestMismatch` already
  classified to `DataError` in v0.5.8 (`git show v0.5.8:…/client/error.rs:217`).
  0.6.0 splits out a *distinct* new variant, also 65
  (`error.rs:47`: *"Distinct from `Self::DigestMismatch`, which is what this used
  to be"*). No mirror-visible change.
- **`NotAManifest`, `TraversalLimitExceeded`, `UnsafeDestination`,
  `UnfollowedRedirect`.** All classify to `DataError` (65)
  (`error.rs:357-361`). Additive.

### 9e. New `OCX_*` env keys are not forwarded to subprocesses — **UNCERTAIN, owner call**

Mechanically diffing the `"OCX_…"` literals in `env.rs` between the two tags:
**four keys added, none removed** — `OCX_IDENTITY_TOKEN`, `OCX_KEY_PASSWORD`,
`OCX_NO_VERIFY`, `OCX_SIGNING_KEY`.

`src/pipeline/ocx_cli.rs:41-57` forwards a **fixed allow-list** of 13 vars; none
of the four is in it. So an operator cannot supply a signing key, or disable
auto-verify, for the mirror's `ocx package push` / `announce` legs.

**Default behaviour is unaffected.** Auto-verify is policy-gated —
`external/ocx/crates/ocx_lib/src/package_manager/tasks/auto_verify.rs:180-186`:

```rust
if policies.is_empty() {
    crate::log::info!(
        "no trust policy covers '{target}'; installing '{resolved}' without signature verification"
    );
    return Ok(());
}
```

With no trust policy configured it is a no-op, so nothing changes today.

**Owner call:** does ocx-mirror intend to support signing its pushes, or running
under a fleet trust policy? If yes, add `OCX_SIGNING_KEY`, `OCX_KEY_PASSWORD`,
`OCX_IDENTITY_TOKEN`, `OCX_NO_VERIFY` to `OCX_VARS`. If no, the omission is
correct and should be stated in a comment so the next `OCX_*` addition is a
deliberate decision rather than a silent gap.

---

## Dependency feature-list drift

Mechanical diff of `[workspace.dependencies]` between
`external/ocx` v0.5.8 and v0.6.0, cross-checked against
`/home/mherwig/dev/ocx-mirror/Cargo.toml`:

| dep | ocx 0.6.0 | mirror | status |
|---|---|---|---|
| `reqwest` | `{ version = "0.13", default-features = false, features = ["json","rustls","charset","http2"] }` | identical | **synced** (already done) |
| `url` | `{ version = "2.5.8", features = ["serde"] }` — **new in 0.6.0** | identical | **already matches** |
| `tokio` | `{ version = "1.52", features = ["full"] }` | identical | match |
| `clap` | `{ version = ">=4.5.57, <5", features = ["derive","color"] }` | identical | match |
| `serde` | `{ version = "1.0.228", features = ["derive"] }` | identical | match |
| `sha2` `0.11.0`, `chrono` `0.4.44`, `regex` `1.12.3`, `bytes` `1.11.1`, `futures` `0.3.32`, `tempfile` `3.27.0`, `tracing` `0.1.44`, `hex` `0.4.3`, `anyhow` `1.0.102`, `serde_yaml_ng` `0.10.0`, `schemars` `1.2.1` | unchanged from v0.5.8 | identical | match |
| `serde_json` | `"1.0.150"` | `{ version = "1.0.150", features = ["preserve_order"] }` | **divergent — pre-existing, not a 0.6.0 item** |
| `rustls`, `octocrab`, `sha1`, `md-5`, `quick-junit` | absent from ocx | — | mirror-owned, correct |

**No dependency bump is required by the 0.6.0 adoption.** Two notes:

1. **CLAUDE.md is stale.** It states *"`octocrab` and `url` are mirror-owned
   outright — no ocx equivalent exists to sync against."* ocx 0.6.0 now declares
   `url` in `[workspace.dependencies]`. The values happen to be byte-identical, so
   nothing breaks, but `url` has moved from "mirror-owned" to "shared, keep in
   sync" and the doc should say so. `octocrab` remains mirror-owned.
2. **`serde_json`'s `preserve_order` is a pre-existing divergence** (v0.5.8 also
   had bare `serde_json`). Because Cargo unifies features graph-wide, the mirror's
   `preserve_order` flips `serde_json::Map` from `BTreeMap` to `IndexMap` for
   `ocx_lib` too — the DATA-DET-03 hazard. Out of scope for this bump; worth an
   issue.

---

## Actions required

1. **`src/command/package/mod.rs:84`** — resolve the plain-HTTP allowance through
   `ocx_lib::insecure_hosts(&config, &env)` over a loaded config, not the raw env
   list. Fixes both the fails-closed (config-declared `insecure = true` ignored)
   and fails-open (system-scope lock ignored) directions. [item 1]
2. **`src/command/package/pipeline/push/alias.rs:158`** — the `sha256.` prefix
   filter no longer matches ocx's digest tags. Replace it with
   `ocx_lib::package::tag::Tag::is_reserved_str(tag)`, which covers the new and
   legacy forms plus cosign sidecar tags, and update the fixture at
   `src/command/package/pipeline/push/tests/ordering.rs:9`. [item 9a]
3. **Owner decision** — `src/pipeline/registry_copy.rs:999-1018` writes
   `sha256.<hex>` while ocx 0.6.0 writes `__ocx.keep.<alg>-<hex>`. Follow ocx, or
   document the divergence. Touches the `canonical_tags:` spec key
   (`src/spec/registry.rs:114-133`) and four acceptance tests. [item 9a]
4. **Confirm canonical-registry reachability** for the python-env push leg: the
   any-pin provenance read no longer honours `OCX_MIRRORS` and is fail-closed.
   [item 3]
5. **Owner decision** — add the four new signing/verify `OCX_*` vars to
   `src/pipeline/ocx_cli.rs:41` `OCX_VARS`, or comment the deliberate omission.
   [item 9e]
6. **Doc fix** — CLAUDE.md: `url` is no longer mirror-owned; ocx 0.6.0 declares it.
7. **No action** — items 2, 5, 6, 7, 9b–9d. Dropping `--new` (item 4) is a strict
   improvement with no new first-publish failure mode.
