# Research: supply-chain trust for a rewriting registry mirror

**Axis:** security & compliance
**Date:** 2026-08-13
**For:** `adr_registry_mirror_sync.md` (`ocx-mirror registry sync`)
**Scope:** what trust properties survive a mirror that copies OCI content by
digest and rewrites each index root's `repository` field.

---

## The crux

**Digest-pinning secures a transfer, not a claim.**

Fetching by digest means hash(bytes) == digest is enforced by every OCI client:
no in-flight substitution, no bit-rot, no MITM swap. That is real and it is the
whole of what "digests are preserved" buys.

But in this design the consumer's **only** record of *which digest is correct for
a tag* is the mirror's own rewritten index — `[registries."<ns>"] index = "…"`
points straight at it. **The mirrored index is the trust root, not a cache of
one.** Anyone who can write to that git repo or static host can assert a new
digest for any tag, and nothing downstream holds an independent opinion to
contradict it.

This is exactly the result TUF grew out of: Cappos et al.,
[*A Look in the Mirror: Attacks on Package Managers*](https://theupdateframework.io/papers/attacks-on-package-managers-ccs2008.pdf)
(CCS 2008) — APT and YUM were already GPG-signing individual packages and were
still fully exploitable by a malicious mirror, because **signature verification
validates package content, not delivery semantics.** Substitute "OCI blob" for
"package" and the paper describes this design.

---

## 1. TUF attack taxonomy, mapped

Definitions from the [TUF specification](https://theupdateframework.github.io/specification/latest/)
and [theupdateframework.io/docs/security](https://theupdateframework.io/docs/security/).

| Attack | Stopped by digest-pinning? | Why |
|---|---|---|
| Wrong-software-installation, **in flight** | **Yes** | Content fetched by digest; any altered byte changes the hash |
| Wrong-software-installation, **at the index** | **No** | The mirror's index *is* the record of the correct digest. Editing it is authorship, not substitution — nothing external contradicts it |
| **Rollback** | **No** | An old, genuinely-once-real digest is a valid digest. Nothing enforces per-tag monotonicity |
| **Indefinite freeze** | **No** | A static index carries no expiry, no sequence number. A mirror that stopped syncing is indistinguishable from a current one |
| **Mix-and-match** | **No** | Packages sync independently at different times; nothing binds "this set of tag→digest pairs was true simultaneously" |
| Fast-forward | n/a | No version counter in the scheme |

Note that substituting a *different, validly-signed-upstream* package requires
forging nothing at all — it is rollback. Signing attests authorship, never
recency.

TUF's stated premise is the threat model this design needs to name: *"The
framework has very little trust in repositories,"* assuming *"an adversary who
can respond to client requests, whether by acting as a man-in-the-middle or
through compromising repository mirrors."*

---

## 2. What comparable ecosystems do — the recurring pattern

**Every ecosystem that lets a mirror rewrite locations pairs that freedom with a
signature or transparency log that does not originate from the mirror.**

| Ecosystem | Independent artifact | What keeps the mirror honest |
|---|---|---|
| **Debian/apt** | `Release`/`InRelease`, GPG-signed by the archive key, `Valid-Until` | Signature verified against a locally trusted key regardless of which mirror answered. Caveat: [the Debian wiki](https://wiki.debian.org/DebianRepository/Format) says client behaviour on an expired `Release` is *unspecified* |
| **Alpine** | Signed `APKINDEX.tar.gz`, key distributed out-of-band to `/etc/apk/keys` | Same shape |
| **RPM/dnf** | Signed `repomd.xml` + `gpgkey=` | Same shape |
| **Go modules** | **GOSUMDB** — a Merkle transparency log, deliberately separate from the proxy | The crux pattern, by name: `GOPROXY` is untrusted *by construction* precisely because `go.sum` is cross-checked against a log the proxy operator does not control ([design/25530-sumdb](https://go.googlesource.com/proposal/+/master/design/25530-sumdb.md)) |
| **crates.io sparse index** | **None** | The negative example — a mirror rewrites `dl` in `config.json` and cargo simply trusts it ([registry-index docs](https://doc.rust-lang.org/cargo/reference/registry-index.html)). Tolerable only because crates.io is centrally operated; not a model to copy |
| **Nix** | `.narinfo` `Sig`, checked against local `trusted-public-keys` | Trust is 100% in the key, 0% in the URL |
| **Homebrew** | SHA-256 in a human-reviewed formula PR, plus Sigstore build-provenance attestations | [docs.brew.sh](https://docs.brew.sh/Homebrew-Security-and-Supply-Chain): a malicious bottle "would not match the sha256 checksum recorded in the GitHub-hosted repository, and changing that checksum requires a pull request reviewed by a human" — human review doing TUF's root-role job |
| **PyPI (PEP 458)** | TUF roles retrofitted onto the existing unsigned static index, URLs unchanged | **Closest precedent for this exact retrofit problem** — bolting signed freshness onto a shipping static index without a breaking change ([PEP 458](https://peps.python.org/pep-0458/)) |

---

## 3. Sigstore / cosign / referrers under a copy

**(a) Signatures survive a correct copy.** A cosign signature is over the
manifest digest, not a registry location, so a digest-preserving copy keeps it
valid. Signatures live in the same repo by default, stored via
[OCI 1.1 referrers](https://opencontainers.org/posts/blog/2024-03-13-image-and-distribution-1-1/)
(a `subject` on the signature manifest) with a `sha256-<digest>` fallback tag for
pre-1.1 registries ([sigstore registry-support](https://docs.sigstore.dev/cosign/system_config/registry_support/)).
`oras cp --recursive` exists specifically to walk that referrers graph across
registries ([oras cp](https://oras.land/docs/commands/oras_cp/)).

**(b) They do not travel by default — and the failure is silent.** Harbor
[#23210](https://github.com/goharbor/harbor/issues/23210): OCI 1.1 referrers
(cosign v3 signatures, SBOMs) not replicated Artifactory→Harbor, because the
replication path copied tagged manifests and not the referrers graph.

> **Design requirement, not a trust question:** unless the sync path walks
> `/v2/<name>/referrers/<digest>` (and the fallback tag) and copies those
> artifacts too, mirrored packages **silently lose every signature and
> attestation** with no error. The copy "succeeds", the image is byte-identical,
> and the referrer simply never appears downstream. Directly blocks
> [ocx-mirror#7](https://github.com/ocx-sh/ocx-mirror/issues/7).

**(c) And none of it protects `repository`.** A preserved cosign signature
attests "this manifest digest was signed by key K." It says nothing about which
repository should be trusted to serve it. The rewritten `repository` is a plain
unsigned JSON field in an OCX-native document one layer *above* OCI — an attacker
editing it to point at a registry they control, and publishing their own
(possibly validly signed) image there, is untouched by every mechanism in (a).

---

## 4. Freshness — the cheapest credible fix

Generalized primitive across §2: **monotonic sequence + signed timestamp +
explicit expiry, checked client-side, failing closed.** TUF isolates this in its
short-lived **timestamp role**; Debian spells it `Valid-Until`; the Go sumdb's
append-only log makes silent rewrite of a past checksum detectable instead.

Cheapest version here, with no PKI bootstrap: one small signed manifest at the
root of the published tree carrying `sequence`, `published_at`, `valid_until`,
signed with a key the org already has (cosign keyless via CI OIDC, or a
checked-in age/SSH key — this is an internal tool, not a public CA problem).
Consumers fail closed past `valid_until`.

**Cost to be honest about:** enforcement requires a *consumer-side* change in
`ocx`, not just a mirror-side write. Unenforced metadata is decoration. This is
therefore a follow-on ADR with an ocx dependency, not a v1 mirror feature.

---

## 5. Blob-mount dedup and tenant isolation

Real prior art, and it resolves the question: **cross-repository blob mount is
safe only when gated by the same read authorization a normal pull of the source
would require.**

- The [OCI distribution-spec](https://github.com/opencontainers/distribution-spec/blob/main/spec.md)
  defines `?mount=<digest>&from=<other_name>` and is **silent on authorization** —
  which is why implementations diverged.
- **GHCR (2022, disclosed 2023)** — implicitly cross-mounted blobs from any
  repository without checking the requester's access to the source, leaking
  private repository existence and names via storage-backend metadata in response
  headers. Bytes stayed protected; repo-identity confidentiality did not.
  ([Chainguard writeup](https://www.chainguard.dev/unchained/ghcr-private-repos-sometimes-werent))
- **AWS ECR** does it correctly: mounting requires `ecr:GetDownloadUrlForLayer`
  on the **source repository specifically** ([blob-mounting docs](https://docs.aws.amazon.com/AmazonECR/latest/userguide/blob-mounting.html)).

**Bearing on this design.** Both are *registry-side* properties — as a mount
client the mirror cannot cause them, only be refused. What does bear on us is the
`blob_anchor` ACL question, and the distinction the GHCR/ECR split draws is the
useful one: opt-in **at the feature level** (a global boolean) is the GHCR
anti-pattern; opt-in **at the source-repo level** (one named repository) is the
ECR shape. The design's `blob_anchor` is a single named repository, which is
already the correct shape — but it should say so, rather than reading as a
feature flag.

---

## 6. Credential forwarding on the push path — a named CVE class

- **[GHSA-jxpm-75mh-9fp7](https://github.com/oras-project/oras-go/security/advisories/GHSA-jxpm-75mh-9fp7)**
  — oras-go ≤ v2.6.0, **CVSS 7.5, CWE-918**. `completePushAfterInitialPost`
  followed a registry-controlled `Location` on a monolithic blob upload and
  reused the initial `POST`'s `Authorization` on the follow-up `PUT`, with no
  same-host check. A malicious or compromised registry returns a cross-host
  redirect and harvests the credential. Fixed in v2.6.2 by validating `Location`
  against the original request and never forwarding `Authorization` when the
  upload target changes host or scheme.
- **CVE-2025-27119** (cdxgen) — same class on the pull side: credential selection
  by substring match (`serverAddress.includes(forRegistry)`) instead of exact
  host match.
- **The spec already mandates the rule**, verbatim from
  `opencontainers/distribution-spec/spec.md`: *"clients SHOULD follow such
  redirects, and MUST NOT forward `Authorization` headers across host boundaries
  unless explicitly configured to do so."* And separately: *"Authorization
  credentials for an upstream registry SHOULD NOT be sent to a proxy registry
  unless explicitly configured or instructed to do so by the credential owner."*

This repo is directly in that blast radius —
`oci-client = { path = "external/ocx/external/rust-oci-client" }` is the ocx-sh
fork of [oras-project/rust-oci-client](https://github.com/oras-project/rust-oci-client)
(`Cargo.toml:90`). It is the same defect tracked as
[ocx#272](https://github.com/ocx-sh/ocx/issues/272).

**Rule:** never reuse a bearer token or `Authorization` header across a
scheme/host/port change unless that exact target was allow-listed by the caller
*before* the request. Validate the redirect target against the original origin
before attaching credentials, not after.

---

## Recommendation

**v1, cheap and mechanical:**

1. **Referrers-aware copy** (§3b) — walk `/v2/<name>/referrers/<digest>` and the
   fallback tag, or signatures and attestations vanish silently.
2. **Exact-origin check before forwarding any credential** on a redirect or
   upload-session `Location` (§6) = [ocx#272](https://github.com/ocx-sh/ocx/issues/272),
   which the distribution spec already requires in normative language.
3. **Document `blob_anchor` as a named-source-repo opt-in**, not a feature flag
   (§5).

**Must not claim.** That digest-pinning gives the mirror supply-chain integrity.
It proves bytes were not corrupted in one transfer. It says nothing about whether
the digest shown is current, ever existed as published, or is consistent with any
other package the mirror serves. "Digests are byte-identical" must not read as
"trustworthy" in the docs or in a security review — different claims, and only
the first is true today.

**Documented residual (name it, do not solve it in v1).** The mirror's git repo
and static host *are* the sole root of trust for every consumer pointing `index=`
at them. Anyone able to write there — compromised CI, leaked deploy credential,
malicious insider — can roll back, freeze, or arbitrarily redirect any package,
undetectably, for every consumer. This is architecturally the gap Cappos et al.
broke in 2008 and TUF exists to close; closing it properly needs an offline-keyed
root of trust independent of the mirror operator. The honest framing is not
"defend against an internet attacker" but **"bound the blast radius when the
mirror's own CI or host is compromised" — and today nothing bounds it at all.**
