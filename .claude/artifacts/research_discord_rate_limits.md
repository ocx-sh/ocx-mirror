# Research: Discord Webhook Rate Limits & Rust Retry

> Source for ocx-mirror issue #10 (`embeds_per_message` opt-in one-message-per-version).
> Per-version mode multiplies POST count (30-version backfill → 30 messages). The
> current `discord::post` loop has NO delay and does NOT honor 429. This brief
> grounds the rate-limit-safety design.

## 1. Discord webhook rate limits

- Discord uses **per-route buckets**, identified by `X-RateLimit-Bucket`.
- Webhook execution: community-documented **5 requests / 2 seconds per webhook**
  (birdie0 guide; official developer docs do **not** state a numeric limit for the
  execute-webhook route — a known documentation gap).
- **Shared-bucket risk**: [discord-api-docs#6753](https://github.com/discord/discord-api-docs/issues/6753)
  reports Discord may enforce a single shared bucket across all webhooks in a guild
  (community-wide 5/1s) rather than per-webhook. Treat per-webhook as optimistic,
  guild-shared as the conservative reality — i.e. a 429 can arrive even below the
  per-webhook rate. **Reactive handling is mandatory, not optional.**
- Global ceiling: ~10k requests / 10 min at the Cloudflare layer (informal community
  lore, not an official commitment). For a 30-message backfill the per-webhook bucket
  (5/2s) is the binding constraint, not the global limit.

## 2. The 429 response shape

| Element | Value |
|---|---|
| HTTP status | `429 Too Many Requests` |
| `Retry-After` header | seconds (integer, HTTP standard) |
| JSON body `retry_after` | **seconds as float** (e.g. `0.412`) — sub-second precision |
| JSON body `global` | `true` if global bucket hit |
| `X-RateLimit-Remaining` | requests left in window |
| `X-RateLimit-Reset-After` | seconds until bucket reset (float) — proactive signal on non-429 |
| `X-RateLimit-Scope` | `user` / `global` / `shared` (only on 429) |

**Authoritative sleep source**: prefer JSON body `retry_after` (float seconds, Discord
canonical, sub-second). Fall back to the `Retry-After` header (integer seconds) when the
body is absent/unparseable.

## 3. Recommended client behavior — both layers

- **Proactive pacing**: fixed inter-message delay between successive POSTs. At 5 req/2s
  the floor is 1 msg / 400ms; **750ms–1000ms** gives a 1.5–2× margin (a 30-message
  backfill completes in ~22–30s). Apply only when more than one message is sent.
- **Reactive backoff**: on any 429, sleep `retry_after` (+ ~150ms jitter), then retry.
  **Cap retries at 3** per message; on exhaustion surface an error — never silently drop
  a notification.
- Keep the reactive layer even with pacing, because of the guild-shared-bucket behavior.

## 4. Rust impl notes (`reqwest` + `tokio`)

```rust
// reactive: prefer JSON body retry_after (float seconds), fall back to header
if response.status() == StatusCode::TOO_MANY_REQUESTS {
    let retry_after = read_retry_after(&response).await; // body.retry_after | header | default
    tokio::time::sleep(Duration::from_secs_f64(retry_after + 0.15)).await;
    // retry, bounded by a 3-attempt cap
}
```

```rust
// proactive: between messages in the notify execute loop (not inside post())
tokio::time::sleep(INTER_MESSAGE_DELAY).await;
```

- Read header via `response.headers().get("retry-after")`; sleep via `tokio::time::sleep`
  (never `std::thread::sleep` in async — see quality-rust.md).
- Hard ceiling per sleep to avoid an adversarial/unbounded `retry_after`.
- **Separation of concerns**: reactive 429 retry lives in `discord::post` (it owns the
  HTTP response); proactive inter-message pacing lives in the `notify::execute` loop
  (it owns the cadence across messages).

## Uncertainty flags

- Exact per-webhook numeric limit (5/2s vs 30/min) is community-sourced, not in official
  docs. 5/2s is the conservative, widely-trusted figure.
- Guild-shared-bucket behavior (#6753) may be unresolved — treat as a live risk.
- 10k/10min Cloudflare ceiling is informal.

## Sources

- <https://docs.discord.com/developers/topics/rate-limits> — official: 429 shape, headers, units, global limit
- <https://birdie0.github.io/discord-webhooks-guide/other/rate_limits.html> — 5 req/2s per webhook, proactive header checks
- <https://github.com/discord/discord-api-docs/issues/6753> — guild-shared bucket report (2024)
