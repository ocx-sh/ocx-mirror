# Research: lock portability and wire compatibility under a rewritten `repository`

**Axis:** data model / compatibility
**Date:** 2026-08-13
**For:** `adr_registry_mirror_sync.md` (`ocx-mirror registry sync`)
**Trees read:** `/home/mherwig/dev/ocx` (v0.5.8 main), `/home/mherwig/dev/index`

---

## VERDICT

**A rewritten `repository` satisfies R4. `ocx.lock` stays portable between a
mirrored host and a direct-egress host — byte-identically, not merely
compatibly.**

`ocx.lock` records **logical identity only**; the mirror's rewrite lives
exclusively in *index* documents, which the lock never embeds. This is not
incidental — an ADR proposing the opposite was written and **rejected by the
owner** on exactly these grounds.

### Evidence chain

| Claim | Evidence |
|---|---|
| Lock schema is logical | `LockedTool.repository: Identifier` — *"Bare registry/repo coordinates shared by every platform leaf"* (`ocx_lib/src/project/lock.rs:170`). Fixture value `ocx.sh/cmake` (`lock.rs:703`) — a logical registry name, not a physical host |
| Write site confirms it | `resolve_one` writes `repository: identifier.without_specifiers()` (`resolve.rs:516`), where `identifier` comes from `ocx.toml` via `declared_identifier` (`resolve.rs:463-469`). No physical lookup at lock-write time |
| The alternative was considered and refused | `.claude/artifacts/adr_lock_records_physical_address.md` — **Status: Rejected, 2026-07-30**. Verbatim: *"recording it would pin routing and stop a locked project following a registry migration, where re-deriving follows it and the digest still pins the bytes"* (lines 15-17). The owner reasoning through this exact scenario |
| Install re-derives physically at resolve time | `resolve_transport_pinned` (`resolve.rs:466-478`) → `index.physical_reference(...)` → `LocalIndex::physical_reference` (`local_index.rs:731-748`) reads the **local cached** root and parses `repository` via `parse_physical_repository`. Zero network when the index is warm |
| Storage keys on logical identity too | `blobs/`, `layers/`, `packages/`, `tags/`, `symlinks/` all key on `identifier.registry()` — so a machine switching between mirrored and direct-egress config for the same logical identity reuses one on-disk cache |
| No contradiction with existing design | `adr_index_indirection.md:367-368`, Decision C: *"storage paths, `ocx.lock`, and GC roots key on LOGICAL identity… never physical."* `repository` is explicitly a routing pointer expected to be rewritten (`indices.md:174-176`: *"root.repository → mirror_map → fetch"*) |

**Precondition to state in the ADR:** the resolving host's own index must hold a
current root for the name. True regardless of mirroring — but it means a
mirrored-only host will fail to resolve a package the mirror never copied, even
though the same package resolves fine publicly.

---

## Wire-format skew across a mixed fleet

Source: `adr_servable_index_snapshot.md` (ratified 2026-08-09, shipped v0.5.8).

- **Decision A** — absent `config.json` ⇒ `format_version 1` at every reader,
  local and remote alike; an unrecognized version is a hard error (exit 65) at
  every reader. `SUPPORTED_FORMAT_VERSION: u64 = 1` (`wire.rs:49`), `!=`
  comparison (`wire.rs:67-71`).
- **Decision F** — one `PythonJson` formatter (2-space indent, struct field
  order, `\uXXXX` non-ASCII escapes, trailing newline) shared by
  `serialize_root` / `serialize_catalog` / `serialize_config`.

**Direction matrix:**

| Reader | Tree | Outcome |
|---|---|---|
| ocx < 0.5.8 | mirror tree **with** `config.json` | Fine — the "Found" arm was always correct; only absent-arm semantics changed |
| ocx < 0.5.8 | mirror tree **without** `config.json` | **Silent failure** — read as `NotAnIndex`, resolve returns `Ok(None)`, every package reports not-found with no diagnostic |
| ocx ≥ 0.5.8 | any tree | Fine under the uniform rule |
| any | unrecognized `format_version` | Hard error 65, both readers, flag-day by design (Known tension #2: *"no way to tell an old client it is too old"*) |

> **Requirement:** the mirror must **always** write `config.json`. And it must
> never be the first mover on bumping `format_version` without fleet-wide
> coordination.

**Unknown fields are safe to carry.** `IndexRoot` has no `deny_unknown_fields`
anywhere — *"index documents are read by many client versions at once and must
tolerate newer fields (fleet forward-compat)"* (`wire.rs:141-143`), pinned by a
unit test at `wire.rs:477-491`. The mirror can pass through fields this ocx
version does not model.

**Use the shipped serializer, not a hand-rolled one.**
`wire_writer::serialize_root(root: &serde_json::Value) -> Vec<u8>` is public
(`wire_writer.rs:59`) and takes a raw `Value` — so the rewrite is: parse, mutate
the single `repository` key, re-serialize. Byte-exactness is a
cross-implementation parity concern rather than a correctness one, but
`quality-core.md` flags hand-rolled serializers Block-tier, and the ADR's own
Known tension #1 records a near-identical historical `ensure_ascii`
escape-boundary bug.

**`config.json` has no repair path, by design** (Known tension #4: write-if-
absent, never updated, `regenerate` never touches it). Treat it as a one-time
bootstrap artifact; repair is delete + re-run.

---

## Location-dependent fields in the root document

Read against the live `/home/mherwig/dev/index/p/kubernetes/kubectl.json`.

**Reframing fact:** ocx's `IndexRoot` parser models **only** `repository`,
`tags`, `status`, `deprecated_message`, `superseded_by` (`wire.rs:144-159`). It
does not model `name`, `owners`, `desc`, `upstream`, `created` at all — those
pass through unexamined and are inert to `ocx install` / `ocx run`, existing for
the catalog/website layer.

| Field | Mirror action | Basis |
|---|---|---|
| `repository` | **Rewrite** | The one field the design turns on |
| `name` | Copy verbatim | Logical identity — rewriting it breaks R4 |
| `owners`, `created`, `upstream.*`, `desc.{digest,title,description,keywords}` | Copy verbatim | Catalog-only, not modeled by ocx |
| `status` / `deprecated_message` / `superseded_by` | Copy verbatim, never fabricate | **Consumed by ocx** — drives yank/deprecation warnings on every resolve (`surface_root_status`, `ocx_index.rs:851`) |
| `tags{content, observed}` | Copy byte-identically | The digest pins |
| `desc.readme` / `desc.logo` | **See gap below** | |

### The `__ocx.desc` gap — concrete, and it will not fix itself

`ocx package describe` (`ocx_cli/src/command/package_describe.rs:63-125`) calls
`publisher.push_description(&identifier, &desc)` with the package's **own** OCI
identifier. `Publisher::push_description` / `pull_description`
(`ocx_lib/src/publisher.rs:215-226`): *"Pull the existing description from the
`__ocx.desc` tag."*

So readme and logo bytes are an OCI manifest plus blobs at
`<repository>:__ocx.desc`, inside the package's own physical repository — not a
separate store, not the index tree.

And `__ocx.desc` is explicitly classified as an **administrative / ignorable
tag**, excluded from version aliasing in the cascade graph builder
(`package/cascade/graph/tests.rs:468,1057`). **Any mirror walking `root.tags{}`
— or reusing ocx's own tag classification — skips it automatically.** That is
what the classifier is for.

**Consequence:** copying only what `root.tags{}` reaches produces a root document
that looks correct and validates, while nothing ever pushed `__ocx.desc` to the
destination. Anything resolving description blobs at `repository` +
`__ocx.desc` gets a 404.

**Correction to an earlier assumption:** `ocx package info` does **not** break —
`desc` is not modeled by ocx at any point on the install path (no reference in
`package_info.rs` or `status.rs`). The breakage is confined to whatever renders
descriptions for humans: a catalog or website view, if the fleet builds one.

**Fix:** explicitly copy the `__ocx.desc` tag (manifest + readme/logo blobs)
alongside the version tags, per mirrored package.

---

## External precedent

**Cargo** keeps `Cargo.lock` valid under `[source] replace-with` because
`source` names a *source identity* and `checksum` (SHA-256 of the `.crate`) is
the portability anchor, verified after fetch regardless of which URL served the
bytes — structurally what ocx does. The parallel is imperfect:
[rust-lang/cargo#15663](https://github.com/rust-lang/cargo/issues/15663) is a
live complaint that `source` is a stable identity users expect to double as a
resolution hint, which replacement defeats. ocx avoids that specific complaint
by making the split **type-level** — `Identifier` (logical, in the lock) versus
`native::Reference` (physical, transport-only) are different Rust types with one
structurally-enforced conversion seam (`adr_index_indirection.md:391-399`).

**npm is the cautionary tale ocx designed against**
(`adr_lock_records_physical_address.md:156-163`): `package-lock.json`'s
`resolved` is a fully-qualified tarball URL captured at lock time — genuinely
physical, genuinely non-portable
([npm/npm#19578](https://github.com/npm/npm/issues/19578)). **Go** sidesteps it
entirely: `go.sum` holds only `h1:` content hashes, `GOPROXY` is ambient env,
never persisted. **pip** anchors on per-file SHA-256 so `--index-url` is
swappable, though a mirror omitting per-file hashes breaks that.

**None of the four made the lock's location field swappable** — all keep the
digest as the sole portability anchor and treat any recorded location as absent
(Go), overridable (Cargo, npm), or a supplement to a hash (pip). ocx lands in the
same family, arguably cleanest: no location string in the lock at all.

Sources: [Cargo source replacement](https://doc.rust-lang.org/cargo/reference/source-replacement.html) ·
[cargo#15663](https://github.com/rust-lang/cargo/issues/15663) ·
[npm/npm#19578](https://github.com/npm/npm/issues/19578) ·
[PEP 691](https://peps.python.org/pep-0691/) ·
[Go modules reference](https://go.dev/ref/mod)

---

## Accuracy flag

`adr_oci_registry_mirror.md` exists only in the vendored submodule
(`external/ocx/.claude/artifacts/`), not in ocx-mirror's own artifact home. Its
metadata reads **Status: Accepted**, not deprecated — and
`adr_servable_index_snapshot.md` instructs that its `Superseded By` be set for
the index-tree half only, an amendment the vendored copy does not yet carry
(`Superseded By: N/A`). The owner has verbally deprecated it; the file does not
say so. Worth reconciling when the submodule is bumped.
