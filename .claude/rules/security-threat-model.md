---
paths:
  - src/**
  - crates/**
  - tests/**
  - test/**
  - .claude/**
---

# Threat Model (ocx-mirror)

Mirror-native rule. **Owner ruling, 2026-08-14.** Defines who this project defends
against. Every security review — human, `/security-review`, `worker-reviewer` in
`security` focus, or a cross-model adversary pass — is scoped by this file. A
finding outside the boundary below is not a finding; a finding inside it is not
excused by anything here.

## The boundary, in one line

**Defend against outside attackers. Do not question the integrity of the
execution environment.**

## Out of scope — the execution environment is trusted

Assume as sound, and do not raise findings that depend on subverting them:

- The machine or CI runner executing `ocx-mirror`, its filesystem, its
  environment variables, its process memory.
- The operator's own credentials, credential store, and config distribution
  (including the managed-config tier and `/etc/ocx/config.toml`).
- Whoever can write to this tool's **outputs**: the generated index tree, the git
  repository holding it, the static host serving it, the destination registry.
- Insider threat, compromised CI, leaked deploy keys, a malicious maintainer.

**Why.** An actor with any of those already controls everything downstream of
this tool. No configuration, validation, or design choice here changes that
outcome, so modelling it produces findings nobody can act on — and it crowds out
the ones they can. This is a deliberate scope decision, not an oversight.

**Consequence for the mirror specifically:** the corporate mirror's host and CI
*are* the fleet's root of trust, and that is accepted, not mitigated. Rollback,
freeze, and mix-and-match by whoever controls the mirror are **out of scope** for
this project. Provenance arrives later with signing over the referrers API, and
that is signing's problem to solve, not this tool's
(`adr_registry_mirror_sync.md`, Open question 3).

## In scope — everything that reaches us from outside

The attacker is anyone who can influence bytes we **receive** or hosts we
**contact**. All of the following remain first-class security findings:

| Surface | The attacker | Example |
|---|---|---|
| Upstream index / catalog documents | A compromised or malicious source registry | A catalog key of `foo/../../prod-images` escaping a destination path prefix |
| A root document's `repository` pointer | Same | `oci://169.254.169.254` or `oci://vault.internal:8200`, dialled from inside the perimeter — SSRF from foreign data |
| Any registry we contact | A malicious, compromised, or MITM'd registry | A `Location` redirect to an attacker host that harvests the `Authorization` header |
| Manifests, configs, layers, referrers | Upstream | Digest mismatch; unbounded or cyclic referrer graphs; decompression bombs |
| Bytes we republish under our own origin | Upstream | A readme or logo blob copied verbatim and rendered by an internal catalog UI |
| Anything leaving the machine | — | A credential written into output, or into a log that is shipped off-host |

**The rule of thumb:** ask *who* the attacker is. If the answer is "someone who
already has write access to our machine, our CI, or our published output", it is
out of scope. If the answer is "the upstream we mirror", "the network", or "a
registry answering our request", it is in scope and must be handled.

## What this ruling does NOT excuse

Scoping out the execution environment is not a blanket exemption. Still required:

- **Validate every input that crossed the network** before it reaches a path, a
  URL, a filesystem write, or a subprocess argument. Foreign data is foreign
  regardless of how trusted the machine handling it is.
- **Never send credentials outside the intended origin.** A redirect, a proxy, or
  a cross-host upload target is an outside party even when the request began on a
  trusted machine. Validate the target before attaching credentials, never after.
- **Verify content against its digest** on receipt. Do not delegate integrity to
  the peer that supplied the bytes.
- **Keep secrets out of anything durable** — output trees, generated files, error
  messages, logs. Logs leave the trusted environment routinely; a secret in a log
  is a secret outside the boundary. Report the offending key or variable name,
  never the value.
- **Fail closed on malformed foreign input.** A parse that gives up and continues
  is how foreign data becomes control flow.

## For reviewers

State the attacker in every finding. A finding without a named attacker and a
concrete path from that attacker to the effect is noise, and under this model it
is likely out of scope. When a finding's only attacker is an insider or the CI
itself, say so and drop it rather than filing it with a hedge.

## See Also

- `.claude/rules/quality-core.md` — universal Block-tier anti-patterns
  (unvalidated external input at boundaries is one).
- `.claude/artifacts/research_mirror_supply_chain.md` — the TUF taxonomy and the
  ecosystem survey behind the accepted-residual decision.
- `.claude/artifacts/adr_registry_mirror_sync.md` — Open question 3, the trust
  ruling this file generalises.
