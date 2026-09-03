# Mnemosyne Engineering Gotchas

Project-specific pitfalls discovered during development. Each entry exists so
future prompts (potentially with a different AI agent, or the same agent
without this conversation's context) don't silently regress a fix that was
painful to diagnose.

---

## 1. sqlx + Supabase Pooler: `.persistent(false)` on every query

> **STATUS: HISTORICAL — no longer applies as of 2026-08-27.**
>
> Mnemosyne no longer uses Supabase. It now connects directly to the local
> Postgres cluster shared with the Knowledge Store
> (`chiron-ks-postgres.service`, port 5432, its own `mnemosyne` database).
> There is no connection pooler in the path, so sqlx's default
> prepared-statement caching is safe. Every `.persistent(false)` call and the
> `statement_cache_capacity(0)` option have been removed from the codebase.
>
> This is the explicit, recorded decision that the old checklist below asked
> for. **Do not add `.persistent(false)` to new queries.** The entry is kept
> because the diagnosis is still correct and would apply again if a
> transaction-mode pooler (PgBouncer, Supavisor) is ever put in front of the
> database.

### The problem (as it was)

sqlx by default prepares named statements (`sqlx_s_1`, `sqlx_s_2`, ...) and
caches them per connection. Supabase's connection pooler (Supavisor /
PgBouncer in transaction mode, port 6543) does NOT guarantee that a given
client-side "connection" maps to the same Postgres backend connection across
transactions — the pooler may route subsequent statements on the same logical
sqlx connection to a different backend.

When that happens, the new backend still has the prepared statement name
registered from a previous occupant of that backend slot, so the next
`PARSE` of the same name on that backend fails with:

```
ERROR: prepared statement "sqlx_s_N" already exists
```

sqlx wraps this as a `DatabaseError` and surfaces it to the handler; without
explicit mapping it bubbles up as an unhandled 500.

### The symptom

Intermittent HTTP 500 responses with a JSON body like:

```json
{"error":"database error: error returned from database: prepared statement \"sqlx_s_1\" already exists at line 409"}
```

The intermittency is the giveaway: it depends on whether the pooler happens
to route a follow-up query to the same backend that prepared the statement
originally. A handler tested once in isolation may pass; the second invocation
on the same sqlx connection (e.g., the duplicate-email path that does an
`INSERT` both times) is much more likely to collide.

### The fix

Call `.persistent(false)` on every `sqlx::query`, `sqlx::query_as`, and
`sqlx::query_scalar` call made against this database:

```rust
sqlx::query_as::<_, UserRow>("INSERT INTO users (...) VALUES (...) RETURNING ...")
    .persistent(false)              // <-- required for Supabase pooler
    .bind(...)
    .fetch_one(pool.get_ref())
    .await
```

With `persistent(false)`, sqlx uses `StatementId::UNNAMED` (the unnamed
prepared statement) and skips the cache (see
`sqlx-postgres/src/connection/executor.rs` lines 31-37 and 184 in v0.9.0).
The unnamed statement is cleaned up by Postgres at the end of each
transaction, so no name collision can occur across backend reassignment.

This is also the workaround recommended in sqlx's own crate docs
(`sqlx-core/src/query.rs`, `query()` doc, line 538-540): *"Some third-party
databases that speak a supported protocol, e.g. CockroachDB or PGBouncer that
speak Postgres, may have issues with the transparent caching of prepared
statements. If you are having trouble, try setting `.persistent(false)`."*

### What does NOT work

- **`PgConnectOptions::statement_cache_capacity(0)`**: relies on the same
  underlying `persistent` flag in the executor; setting the cache capacity to
  0 alone does not prevent sqlx from preparing a *named* statement on the
  first miss. Verified empirically in Prompt 2 — the same `sqlx_s_N already
  exists` error recurred after applying only this change.
- **Direct-connection URL (`db.<project>.supabase.co:5432`)**: avoids the
  pooler entirely and removes the issue, but Supabase's direct host resolves
  only to IPv6 (AAAA record) as of 2026-07-04, and many dev environments have
  no IPv6 routing. The pooler URL (`aws-0-<region>.pooler.supabase.com:6543`)
  has IPv4 and is what Supabase themselves recommend for application
  connections.

### Rule going forward

The old checklist required `.persistent(false)` on every query and said the
requirement could be relaxed only if the pooler was switched off, by an
explicit decision recorded here. That switch has now happened, and this is
that record:

- Mnemosyne runs against a direct Postgres connection. New `sqlx::query*`
  calls need **no** `.persistent(false)`; write them plainly.
- If a transaction-mode pooler is ever introduced between the backend and
  Postgres, everything above applies again — reinstate `.persistent(false)`
  on every query and update this status banner.

### Where this was discovered

- **Prompt:** Milestone 2, Prompt 2 (CRUD endpoints for users, study_sets, cards)
- **Date:** 2026-07-05
- **First failing endpoint:** `POST /users` with a duplicate email (a second
  INSERT hitting the same cached statement name on a freshly-rotated backend
  connection).
- **PR / commit:** `feat: add minimal CRUD endpoints for users, study_sets, cards`
  (`71ca228`)
- **Affected files at time of fix:** `backend/src/main.rs` (the `/health/db`
  scalar query) and `backend/src/handlers/{users,study_sets,cards}.rs` (all
  six CRUD query calls).

### Where this was retired

- **Date:** 2026-08-27, when Mnemosyne moved off Supabase onto the local
  Postgres cluster shared with the Knowledge Store. All 38 `.persistent(false)`
  calls across nine files and the `statement_cache_capacity(0)` option in
  `main.rs` were removed in that change.
---

## 2. DeepSeek reasoning tokens: truncation is non-deterministic, and often arrives with partial content

`deepseek-v4-flash` is a reasoning model. Reasoning tokens are billed against
the completion budget but never appear in `choices[0].message.content`, so a
call that runs out of budget comes back looking like an ordinary response that
merely happens to be empty — or, worse, cut off mid-sentence. Only
`finish_reason` says otherwise. This is why every provider call is rejected as
`LLMError::Truncated` at the client boundary (`deepseek.rs::to_llm_response`)
instead of each handler checking for itself.

### Measured, not assumed

Same node, same prompt, same model, `temperature` unset — 10 identical calls at
each budget, run 2026-09-03 against the live API:

| `max_tokens` | truncated | reasoning tokens observed | truncated replies carrying partial content |
|---|---|---|---|
| 200 | 10/10 | 200 (pinned at the ceiling) | 0/10 |
| 350 | 10/10 | 232 – 350 | 5/10 (up to 310 chars) |
| 500 | 8/10 | 78 – 500 | 2/8 |
| 650 | 7/10 | 162 – 650 | 2/7 |

Two things follow, and they pull in opposite directions:

**Reasoning consumption is wildly non-deterministic on identical input.** At
`max_tokens=650` the same request spent anywhere from 162 to 650 reasoning
tokens — a 4× spread with nothing varying on our side. Any logic that assumes a
prompt has a stable cost is wrong.

**A truncated reply frequently contains text.** Half the truncated calls at
`max_tokens=350` returned real, non-empty content — a half-written JSON object.
Handed to a parser, that reports a formatting error and sends whoever reads it
looking at the prompt instead of at the budget. This is the concrete failure the
`finish_reason` check prevents, reproduced deliberately rather than argued for.

### Is retrying a truncated call worth anything?

Only near the boundary. Well below it (`max_tokens` 200 and 350) retrying the
same input succeeded **0 times in 20**. In the marginal band (500, 650) the
variance is enough to get through on **2–3 attempts in 10**. So neither
absolute is right: "truncation always repeats" is false, and "just retry" is
false too. One or two retries are worth it at most; the reliable fix is to give
the call more budget or ask it for less.

Note that Mnemosyne sends **no** `max_tokens` at all — the model's own default
applies. Truncation in normal operation therefore means reasoning consumed that
entire default, which is far outside the band measured here. There is no
measurement for that regime; do not assume these retry odds carry over to it.

### How to reproduce

The client sends no `max_tokens`, so it cannot be lowered from outside. Add the
field to `ChatCompletionsRequest` temporarily, or call the API directly with the
prompt `handlers::cards_from_node::build_prompt` produces. Do not commit a
hardcoded budget — the absence of one is deliberate.
