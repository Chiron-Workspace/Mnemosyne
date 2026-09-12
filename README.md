# Mnemosyne

**A cognitive-science-grounded, AI-assisted learning platform.**

Mnemosyne operationalizes four evidence-based learning methodologies — spaced repetition, active recall, Socratic questioning, and the Feynman Technique — as software, using a validated scheduling algorithm (FSRS) and a large language model (DeepSeek) as a content-generation and dialogue partner rather than a black-box "AI tutor."

Built as a portfolio project exploring the intersection of psychology, computer science, and philosophy of education, with an explicit design philosophy of *lifelong learning*.

📄 **[Read the full research case study](docs/case-study.md)** for the design rationale, empirical findings, and methodology.

---

## Why Mnemosyne

Most flashcard apps implement one learning principle (usually spaced repetition) and leave content creation and comprehension checking entirely to the learner. Mnemosyne asks a more specific question: **which parts of a learning pipeline should be handled by a deterministic algorithm, and which parts genuinely benefit from a language model's flexibility** — and builds accordingly.

| Principle | How it's implemented |
|---|---|
| **Spaced Repetition** | [FSRS](https://github.com/open-spaced-repetition/fsrs-rs) (not the older SM-2), chosen after a dedicated algorithm comparison — see [ADR 0001](docs/adr/0001-spaced-repetition-algorithm.md) |
| **Active Recall / SAFMEDS** | AI-generated short-answer flashcards — from source text (`POST /study_sets/{id}/generate_cards`), or from a Knowledge Store concept node the learner has already studied (`POST /cards/from_node`) |
| **Socratic Questioning** | Multi-turn AI dialogue that asks guiding questions and flags misconceptions, rather than lecturing |
| **Feynman Technique** | Learners write free-text explanations; AI scores clarity/completeness/correctness (1–10 each) with actionable feedback |

---

## Status

Mnemosyne is a backend module of the Chiron ecosystem. It is feature-complete for all four methodologies and verified against a live database with real API calls. There is no frontend: sessions are driven through the HTTP API directly, and user-facing UI is the Chiron OS shell's responsibility.

- [x] Milestone 1 — Foundation (research, architecture, FSRS algorithm selection)
- [x] Milestone 2 — Core learning engine (FSRS scheduling wired to a live DB, AI question generation)
- [x] Milestone 3 — Socratic Tutor + Feynman Evaluation
- [x] Milestone 4 — Chiron integration (local Postgres, transcript hand-off to the Knowledge Store)
- [x] Milestone 5 — Knowledge Store card generation loop: `POST /cards/from_node` turns one approved KS concept node into one flashcard, idempotent via a `UNIQUE(set_id, source_node_id)` constraint, with a machine-readable `reason` on every failure so a caller (namely the Knowledge Store's own `card_sync` job) can tell a token-budget truncation apart from a transient network blip. Verified end-to-end against production data, not just fixtures.
- [ ] User authentication (currently a known, documented limitation — see below)

---

## Tech Stack

- **Backend:** Rust, [Actix-web](https://actix.rs/)
- **Database:** PostgreSQL (local cluster shared with the Knowledge Store, own `mnemosyne` database) — separate databases on the same cluster, no cross-database foreign keys; provenance columns like `cards.source_node_id` are a traceability breadcrumb, not an enforced reference
- **Spaced repetition:** [`fsrs`](https://crates.io/crates/fsrs) crate (FSRS v6.6.1)
- **LLM:** [DeepSeek](https://www.deepseek.com/), model `deepseek-v4-flash` — a *reasoning* model whose thinking tokens count against `max_tokens` without appearing in the response. Every call site checks `finish_reason` explicitly for this reason (see `docs/gotchas.md`); a cut-off answer is rejected outright rather than handed to a JSON parser, which would otherwise misreport it as a formatting error
- **Knowledge Store:** local HTTP service (`chiron-ks-http.service`) — the integration is two-way: Mnemosyne hands off session transcripts to it after a Socratic dialogue ends, and it calls back into Mnemosyne's `POST /cards/from_node` (via its own `card_sync` job) to turn a learner-approved concept into a flashcard

---

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for a full diagram distinguishing built-and-verified components from planned ones.

```
Client (coding agent / Chiron OS shell)
           ↓ HTTP
       Actix-web backend ──────────────→ mnemosyne-core (FSRS scheduling wrapper)
           │        │
           │        └───────────────→ DeepSeek API (chat completions — deepseek.rs
           │                          is the only module that knows the wire format)
           ↓ sqlx
   local PostgreSQL (own `mnemosyne` database, same cluster as the Knowledge Store)

Knowledge Store ──POST /cards/from_node──→ Mnemosyne   (its card_sync job turning an
                                                         approved concept node into a card)
Mnemosyne       ──POST /transcripts──────→ Knowledge Store (hand-off when a Socratic
                                                             dialogue ends)
Mnemosyne       ──GET /nodes, /nodes/{id}→ Knowledge Store (reading a concept back to
                                                             build the flashcard prompt)
```

---

## API Overview

| Endpoint | Purpose |
|---|---|
| `GET /health`, `GET /health/db` | Liveness + DB connectivity checks |
| `POST /users`, `GET /users` | User accounts |
| `POST /study_sets`, `GET /study_sets` | Study set (topic) management |
| `POST /cards`, `GET /cards` | Flashcard CRUD |
| `POST /cards/from_node` | Turn one Knowledge Store concept node into one flashcard. Idempotent per `(study_set_id, node_id)` — a repeat call returns `409` with the existing card's id rather than a duplicate. On failure, `502` carries a machine-readable `reason`: `truncated` (the model hit its token budget — retrying the same input won't help), `provider_error` (network/upstream/unparseable — retry with a bound), or `knowledge_store_error` (the node couldn't be read from the Knowledge Store — usually safe to retry) |
| `POST /review` | Submit a card review rating → FSRS reschedules it |
| `GET /due` | Fetch cards due for review right now |
| `POST /study_sets/{id}/generate_cards` | AI-generate flashcards from source text (`recall` or `elaboration` style) |
| `POST /socratic/start`, `POST /socratic/{id}/reply`, `GET /socratic/{id}` | Multi-turn Socratic dialogue on a study set |
| `POST /study_sets/{id}/feynman_evaluate`, `GET .../history` | Submit and score a self-explanation |

Full request/response shapes are documented inline in each handler under `backend/src/handlers/`.

> **Known limitation:** endpoints currently take `user_id` directly as a request parameter — there is no authentication/session layer yet. This is an intentional, documented simplification for a closed 2–3 user alpha, not an oversight.

---

## Getting Started

### Prerequisites
- Rust 1.90+ (`rustup update`)
- A local PostgreSQL cluster — Chiron runs one via `chiron-ks-postgres.service` on port 5432
- A [DeepSeek API](https://platform.deepseek.com/) key

### Setup

1. Clone the repo and copy the environment template:
   ```bash
   cp .env.example .env
   ```
2. Fill in `.env`:
   - `DATABASE_URL` — your local Postgres URI, e.g. `postgresql://postgres@127.0.0.1:5432/mnemosyne`
   - `DEEPSEEK_API_KEY` — from DeepSeek's platform
   - `KS_HTTP_TOKEN` — bearer token for the Knowledge Store HTTP API. Gates two things now, not one: leave it empty and transcript hand-off is skipped with a startup warning (study sessions are otherwise unaffected), but `POST /cards/from_node` will refuse outright with `503` — it has no way to read the concept it's supposed to turn into a card without it.
3. Create the database and apply the schema:
   ```bash
   psql -h 127.0.0.1 -p 5432 -U postgres -c 'CREATE DATABASE mnemosyne'
   psql -h 127.0.0.1 -p 5432 -U postgres -d mnemosyne -f backend/sql/schema.sql
   ```
   `schema.sql` is the fresh-install baseline; check its own header for the migration it was last regenerated from. Migrations newer than that baseline still need applying by hand, in order, e.g.:
   ```bash
   psql -h 127.0.0.1 -p 5432 -U postgres -d mnemosyne -f backend/sql/migrations/0004_add_quiz_tables.sql
   psql -h 127.0.0.1 -p 5432 -U postgres -d mnemosyne -f backend/sql/migrations/0005_add_card_source_tracking.sql
   ```
   These are plain SQL files applied by hand — nothing in the repo tracks which ones have already run against a given database. `0005` in particular is **not** safe to apply twice (its `ADD COLUMN`/`ADD CONSTRAINT` statements have no `IF NOT EXISTS` guard and will error on a second run), so don't re-run a migration once it's landed.
4. Build and run:
   ```bash
   cargo build --workspace
   cargo run -p backend
   ```
5. Verify: `curl localhost:8081/health/db` should return `{"status":"ok","user_count":0}`.

---

## Documentation

- [`docs/research.md`](docs/research.md) — cognitive science literature review underpinning the design
- [`docs/spaced-rep-spike.md`](docs/spaced-rep-spike.md) — FSRS vs. SM-2 comparison
- [`docs/adr/`](docs/adr/) — architecture decision records
- [`docs/architecture.md`](docs/architecture.md) — system diagram
- [`docs/gotchas.md`](docs/gotchas.md) — infrastructure issues discovered during development and their fixes, including the DeepSeek reasoning-token truncation behavior (`finish_reason == "length"` is checked on every call site, not just one — a cut-off answer otherwise looks like an ordinary, merely malformed, response) (the Supabase pooler / prepared-statement entry is retained as history; it no longer applies)
- [`docs/case-study.md`](docs/case-study.md) — full research case study, including adversarial testing of the AI features (sycophancy detection, scoring-discrimination validation)

---

## Development Methodology

This project was built through a structured three-party collaboration: a human developer, an AI acting as technical planner/reviewer, and an AI coding agent ([OpenCode](https://opencode.ai)) doing implementation work. Every schema change was authored but never auto-executed — live database changes were applied only after human review. Every "this works" claim was required to include real, pasted command output rather than a natural-language summary. Details in [`docs/case-study.md`](docs/case-study.md#appendix-development-methodology-note).

---

## License

MIT
