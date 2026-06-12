# Design Spec: [Component Name]

<!--
Component / Feature Design Specification (technical, CLI-focused)
Filename: artifacts/design_spec_[component].md
Owner: Architect (/architect)
Handoff to: /swarm-plan (decompose), /swarm-execute (contract-first TDD)
Related Skills: architect, swarm-plan, swarm-execute

Adapted for ocx-mirror from the ocx template set: this repo ships a headless
CLI, so the spec captures contracts, UX scenarios, and error taxonomy —
exactly what worker-tester needs to write specification tests without
reading any implementation code.
-->

## Overview

**Status:** Draft | In Review | Approved
**Author:** [Name]
**Date:** [YYYY-MM-DD]
**GitHub Issue:** [#N or N/A]
**Related ADR:** [Link or N/A]

[2-3 sentences: what the component does, why it exists, what problem it solves.]

## Design Goals

- [Goal 1]
- [Goal 2]
- [Goal 3]

## Component Contracts

Public API surface with expected behavior per component. Contracts must be
testable — a tester could write failing tests from this section alone.

### [Component 1]

**Purpose:** [What component does]

**Public API:**

```rust
[Types, traits, function signatures]
```

**Behavior:**

| Input / Precondition | Expected Behavior | Postcondition |
|----------------------|-------------------|---------------|
| [Case] | [What happens] | [Observable result] |

### [Component 2]

[Repeat structure above]

## User Experience Scenarios

Action → expected outcome → error cases for each user-facing behavior.

| # | User Action | Expected Outcome | Error Cases |
|---|-------------|------------------|-------------|
| 1 | `ocx-mirror [command…]` | [stdout/exit code/side effects] | [What can go wrong + message] |
| 2 | [Action] | [Outcome] | [Errors] |

## Error Taxonomy

All documented failure modes with remediation guidance. New variants extend
`MirrorError` with an exit-code mapping (`src/error.rs`).

| Failure Mode | Error Variant | Exit Code | Remediation |
|--------------|---------------|-----------|-------------|
| [Mode] | [`MirrorError::…`] | [code] | [What the user does] |

## Edge Cases

Boundary conditions and corner cases enumerated — tester covers these verbatim.

- [Edge case 1]
- [Edge case 2]
- [Edge case 3]

## Trade-off Analysis

Min 2 options, weighted criteria, recommendation with rationale.

| Criterion (weight) | Option A | Option B |
|--------------------|----------|----------|
| [Criterion 1 (n)] | [score/notes] | [score/notes] |
| [Criterion 2 (n)] | [score/notes] | [score/notes] |

**Reversibility:** Two-Way Door | One-Way Door Medium | One-Way Door High
**Recommendation:** [Chosen option + rationale]

## Module Placement

| Change | Location |
|--------|----------|
| [New type/function] | `src/[module]/…` |

## Testing Strategy

| Level | What | Where |
|-------|------|-------|
| Unit | [Component contracts above] | inline `#[cfg(test)]`, fixtures in `tests/fixtures/` |
| Acceptance | [UX scenarios above] | `test/tests/test_*.py` |

## Documentation Impact

| Surface | File | Change |
|---------|------|--------|
| [CLI reference / mirror.yml reference / env vars / changelog] | `docs/…` | [What to update] |

---

## Approval

| Role | Name | Date | Status |
|------|------|------|--------|
| Engineering | | | Pending |
