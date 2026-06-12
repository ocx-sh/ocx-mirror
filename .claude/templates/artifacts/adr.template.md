# ADR: [Decision Title]

<!--
Architecture Decision Record
Filename: artifacts/adr_[topic].md
Owner: Architect (/architect)
Handoff to: /swarm-execute (via plan artifact)
Related Skills: architect, swarm-plan

Format: Based on MADR (Markdown Any Decision Records) - https://adr.github.io/madr/
Best Practices:
- Write ADRs BEFORE implementation commit
- Keep short, specific, comparable across codebase
- One decision per ADR (not groups)
- Quantify when possible (latency budgets, cost envelopes)
-->

## Metadata

**Status:** Proposed | Accepted | Deprecated | Superseded
**Date:** [YYYY-MM-DD]
**Deciders:** [People involved]
**GitHub Issue:** [#N or N/A]
**Related Design Spec:** [Link or N/A]
**Stack Alignment:**
- [ ] Decision fits existing stack (Rust 2024 + Tokio, see CLAUDE.md) and conventions in `.claude/rules/subsystem-mirror.md`
- [ ] OR deviation justified in Rationale section
**Domain Tags:** [pipeline | spec | source | push | ci | security | docs]
**Supersedes:** [adr_topic if applicable]
**Superseded By:** [adr_topic if applicable]

## Context

[Issue motivating this decision or change?]

## Decision Drivers

- [Driver 1: e.g., cascade-tag correctness]
- [Driver 2: e.g., downstream mirror-repo blast radius]
- [Driver 3: e.g., time constraints]
- [Driver 4: e.g., cost considerations]

## Industry Context & Research

[Tech landscape research before decision. Include trending alternatives, adoption signals, design patterns. Reference research artifacts if available.]

**Research artifact:** [`.claude/artifacts/research_[topic].md`](./research_[topic].md) or N/A
**Trending approaches:** [Where industry moving]
**Key insight:** [Top finding driving decision]

## Considered Options

### Option 1: [Name]

**Description:** [Brief description]

| Pros | Cons |
|------|------|
| [Pro 1] | [Con 1] |
| [Pro 2] | [Con 2] |

### Option 2: [Name]

**Description:** [Brief description]

| Pros | Cons |
|------|------|
| [Pro 1] | [Con 1] |
| [Pro 2] | [Con 2] |

### Option 3: [Name]

**Description:** [Brief description]

| Pros | Cons |
|------|------|
| [Pro 1] | [Con 1] |
| [Pro 2] | [Con 2] |

## Decision Outcome

**Chosen Option:** [Option N]

**Rationale:** [Why picked over others]

### Quantified Impact (where applicable)

| Metric | Before | After | Notes |
|--------|--------|-------|-------|
| [Metric] | [X] | [Y] | [Context] |

### Consequences

**Positive:**
- [Consequence 1]
- [Consequence 2]

**Negative:**
- [Consequence 1]
- [Consequence 2]

**Risks:**
- [Risk 1 and mitigation]

## Technical Details

### Architecture

```
[ASCII diagram or description of architecture]
```

### API Contract

```
[Key interfaces, types, or contracts]
```

### Data Model

```
[Key entities and relationships]
```

## Implementation Plan

1. [ ] [Step 1]
2. [ ] [Step 2]
3. [ ] [Step 3]

## Validation

- [ ] Acceptance tests cover decision-relevant behavior
- [ ] Security implications reviewed
- [ ] `task verify` passes on implementation

## Links

- [Related ADR 1](./adr_related.md)
- [External documentation]

---

## Changelog

| Date | Author | Change |
|------|--------|--------|
| [Date] | [Name] | Initial draft |
