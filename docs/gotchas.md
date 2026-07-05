# Mnemosyne Engineering Gotchas

Project-specific pitfalls discovered during development. Each entry exists so
future prompts (potentially with a different AI agent, or the same agent
without this conversation's context) don't silently regress a fix that was
painful to diagnose.

---

## 1. sqlx + Supabase Pooler: must use `.persistent(false)` on every query

### The problem

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

### Rule going forward (checklist for future prompts touching the DB layer)

- [ ] Every new `sqlx::query*` call in this codebase MUST include
      `.persistent(false)` between the SQL string and the first `.bind()`.
- [ ] Code review must reject any PR that adds a `sqlx::query*` call without
      `.persistent(false)` — this is not a style preference, it is a
      correctness requirement against the production DB.
- [ ] If we ever switch off the Supabase pooler (e.g., self-host Postgres
      with direct connections, or move to Supabase's session-mode pooler on
      a different port), this requirement can be relaxed — but that decision
      must be made explicitly and recorded here, not silently.

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