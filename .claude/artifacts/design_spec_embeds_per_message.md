# Design Spec: one Discord message per published version (issue #10)

## Overview

**Status:** Approved (scope reduced per Michael, 2026-06-13)
**GitHub Issue:** #10

`pipeline notify` already builds **one embed per published version**, then batches up to
10 embeds into a single Discord message. This change drops the batching: **each published
version gets its own message** (one embed per message). It is the new and only behavior —
**no configuration knob.**

> The original issue body proposed an opt-in `notify.discord.embeds_per_message` spec field
> plumbed through an `OCX_MIRROR_DISCORD_EMBEDS_PER_MESSAGE` env var. **Declined as YAGNI**
> (Michael): no spec field, no validation, no renderer/template change, no env var, no
> `run-summary.json` change. Just one message per version.

## Scope

**In:**
- `build_messages`: emit one message per version (drop the ≤10 batching).
- Rate-limit safety (correctness on backfills — see C2; droppable).

**Out:**
- `embeds_per_message` spec field, validation, env var, renderer/template change.
- Any `run-summary.json` schema change. Any configurability.

## Component Contracts

### C1 (required): `build_messages` — one message per version

**Location:** `src/command/pipeline/notify.rs`

**Signature (unchanged):**
```rust
fn build_messages(summary: &RunSummary, user_id: Option<&str>) -> Vec<DiscordWebhookPayload>
```

Today: builds `Vec<(DiscordEmbed, is_red)>` (one per version with rows), then
`.chunks(MAX_EMBEDS_PER_MESSAGE=10)` → one payload per chunk. Change: one payload **per
version-embed**. `MAX_EMBEDS_PER_MESSAGE` is removed (moot — one embed per message).

| Input / Precondition | Expected Behavior | Postcondition |
|---|---|---|
| 3 published versions with rows | 3 `DiscordWebhookPayload`s, each with exactly 1 embed | `messages.len() == 3`, every `embeds.len() == 1` |
| Each version's embed | carries that version's own color + title + url (link) | per-version color/link preserved |
| A failed/partial version + `user_id` set | that version's message pings `<@id>` (scoped `allowed_mentions`) | only the failing version's message pings |
| All-green run, `user_id` set | no message pings | `content` / `allowed_mentions` `None` on every message |
| `skipped_existing` version with no rows | yields no message | not counted |
| First emitted embed | keeps author strip + thumbnail; later messages omit (existing decoration rule, now per-message) | only the first message decorated |
| Zero notifiable versions | empty `Vec` → `execute` silent path (no POST) | `Ok(())`, unchanged |

### C2 (recommended, droppable): rate-limit safety

Unconditional one-message-per-version multiplies POST count on backfills (30 versions →
30 messages). Without handling, Discord 429s → current `post` maps 429 → `WebhookUnavailable`
→ `?` aborts `execute` → messages dropped. Two minimal pieces:

- **`discord::post` (`src/discord.rs`)**: on `429`, sleep the `retry_after` (JSON body float
  seconds → `Retry-After` header int seconds → 1.0s default, capped at 60s) + small jitter,
  retry; **3 retries (4 total attempts)**; on exhaustion → `WebhookUnavailable` (exit 69).
  Take the response **by value** when reading the body (`bytes()`/`json()` consume `self`);
  build the `reqwest::Client` once before the loop. 401/403/5xx unchanged.
- **`notify::execute` (`src/command/pipeline/notify.rs`)**: small inter-message delay
  (~750ms) between consecutive POSTs — skip before first / after last, so single-message
  runs incur zero delay.

See [`research_discord_rate_limits.md`](./research_discord_rate_limits.md).

**If dropped:** ship C1 only. Fine for repos that publish 1–3 versions/run; a large backfill
will trip the rate limit and `notify` may fail mid-run.

## Error Taxonomy

| Failure Mode | Variant | Exit | Remediation |
|---|---|---|---|
| (C2) 429 retries exhausted | `MirrorError::WebhookUnavailable` | 69 | re-run `pipeline notify`; backfill in smaller runs |
| (existing) 5xx / timeout | `WebhookUnavailable` | 69 | unchanged |
| (existing) 401/403 | `WebhookPermissionDenied` | 77 | rotate webhook secret |

No new `MirrorError` variants.

## Testing Strategy

**Rewrite existing batching tests** in `notify.rs` (they encode the ≤10 behavior):
- `one_embed_per_version`: 2 versions → now **2 messages**, each 1 embed (was 1 message, 2 embeds).
- `embeds_batch_into_messages_of_at_most_ten`: 11 versions → now **11 messages** (was 2).
- `each_message_with_a_failure_carries_the_ping`: rework to N messages, only the failing version's message pings.
- `only_first_embed_carries_author_and_thumbnail`: rework for one-embed-per-message (first message decorated).
- `skipped_existing_versions_produce_no_embed`: still valid (skipped → no message).

**New (C1):** 3 versions → 3 messages, assert each embed's color+url matches its version
(issue #10 acceptance (a): correctly colored/linked).

**New (C2, if kept):** `discord::post` 429-then-200 → Ok; 429×3-then-200 → Ok (boundary);
429×4 → `WebhookUnavailable`; `retry_after` JSON-body vs `Retry-After`-header vs default;
adversarial `retry_after` clamped. Needs a multi-response stub server carrying headers
(`StubResponse { status, headers, body }`) — existing `one_shot_server` is single-response.
Use `tokio::time::pause()`/`advance()` for sleep assertions (verify `tokio` `test-util`).

**Acceptance** (`test/tests/test_mirror_pipeline.py`): `pipeline notify` over N published
versions → N POSTs; capture ≥1 body asserting 1 embed with expected color/url. Audit the
pre-existing `--webhook-env-var` flag drift in `test_pipeline_notify_stub_is_callable`.

## Documentation Impact

- `CHANGELOG.md`: `feat(notify): one Discord message per published version (#10)`.
- `pipeline notify` reference: note one-message-per-version + (if kept) rate-limit retry/pacing.
- No `mirror.yml` reference change (no new field).
