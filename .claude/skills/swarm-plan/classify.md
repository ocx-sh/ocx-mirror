# Classification Signals

Signal-to-tier map for `/swarm-plan` with tier=`auto`. Also defines overlay triggers stack on chosen tier.

Classifier reads free-text target + GitHub context (PR/issue title, body, labels), picks tier + zero or more overlays. When signals split across adjacent tiers, or overlay mix unusual, mark **low-confidence** — forces meta-plan gate in SKILL.md. Do **not** fire mid-flow `AskUserQuestion`; ambiguity resolved at single approval gate.

## Tier signal table

| Tier | Signals | Examples |
|---|---|---|
| **low** | Two-Way Door; flag/option change; doc edit; fixture addition; single module area; ≤3 files estimated; label `small`, `docs`, `chore` | `add --fail-fast flag to check`, `update getting-started docs`, `add renderer fixture for archive case` |
| **high** | One-Way Door Medium; new subcommand; new spec field; new source type; new pipeline stage; 1–2 module areas; label `enhancement`, `feature` | `new pipeline subcommand`, `add gitlab_release source`, `per-platform version applicability` |
| **max** | One-Way Door High; breaking spec format; cross-module refactor; cascade/push protocol change; generated-workflow template surface change; label `breaking-change`, `epic` | `rework cascade tagging protocol`, `plan.json schema v3`, `refactor orchestrator to event-driven model` |

Prompt may match multiple rows — pick **highest** tier with ≥1 clear signal. Single `low` keyword no demote feature whose body describes high-effort change.

## Confidence rules

- **Confident**: one tier ≥2 matching signals, no competing signals from adjacent tier. Proceed without meta-plan gate.
- **Low-confidence**: signals split across adjacent tiers (e.g., one `low` + one `high`), or target terse with no discriminating cues. Flag classification; SKILL.md routes into meta-plan gate.

Never manufacture question when confident. Rule: *announce and proceed*, or *let meta-plan gate handle it*.

## Overlay triggers

Overlays adjust single axis on top of chosen tier. Stack — multiple triggers may fire. Axis definitions + effects in `overlays.md`.

| Overlay | Triggered by signals |
|---|---|
| `--architect=opus` | "new trait hierarchy", "novel algorithm", "cross-module", "protocol change", "plan.json / run-summary.json schema change" |
| `--research=3` | "new dependency category", "SOTA comparison", "compliance requirement", "security-sensitive area", "cryptographic primitive" |
| `--researcher=haiku` | `--research=1` (single-axis) AND prompt is narrow single-concept factual lookup (e.g., "check if crate X has a CVE", "find the current version of tool Y", "does library Z support feature W"); NO cross-module keywords; NO multi-source synthesis signal. Context-cap guard: if projected research prompt + sources exceeds 150k tokens, stay `sonnet`. |
| `--codex` (plan review) | One-Way Door Medium/High; "public API change"; "breaking change"; "security-sensitive"; "novel algorithm"; label `breaking-change`; label `security`; any One-Way Door costly to reverse |

Defaults per tier (before overlays apply):

| Axis | low | high | max |
|---|---|---|---|
| architect | inline | inline (Two-Way) / sonnet (One-Way) | opus |
| research | skip | 1 | 3 |
| researcher | sonnet | sonnet (→ haiku on narrow-scope trigger) | sonnet |
| codex plan review | off | off (auto-on for One-Way Door) | on (mandatory) |

## GitHub context as a classification input

When `/swarm-plan <N>` resolves to PR or issue, feed into signal matcher alongside free-text prompt:

- Title + body (treat as prompt)
- Labels (map directly to signals — `breaking-change` → `--codex`, `small` → hint toward `low`, `epic` → hint toward `max`)
- For PRs: file list (feeds Discover scope, not classification)

## Examples

1. `/swarm-plan "add --json output to pipeline plan"` → tier=**low**, no overlays, confident.
2. `/swarm-plan "refactor the prepare/push orchestrator"` → tier=**high** + overlays `--architect=opus` (cross-module), `--codex` (protocol surface). Likely promoted to `max` by `--architect=opus` + cross-module combo — depends on prompt details.
3. `/swarm-plan "extend version filtering"` → low-confidence (split between `low` — "extend" — and `high` — "filter semantics change"). Meta-plan gate fires.
4. `/swarm-plan 143` where PR #143 has label `breaking-change` + `enhancement` → tier=**high** + `--codex` overlay (from `breaking-change` label).
