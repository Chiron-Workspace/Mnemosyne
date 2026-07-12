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
| **Active Recall / SAFMEDS** | AI-generated short-answer flashcards (1–5 word answers) from any source text |
| **Socratic Questioning** | Multi-turn AI dialogue that asks guiding questions and flags misconceptions, rather than lecturing |
| **Feynman Technique** | Learners write free-text explanations; AI scores clarity/completeness/correctness (1–10 each) with actionable feedback |

---

## Status

Backend is feature-complete for all four methodologies and verified against a live database with real API calls. Frontend has not been built yet — all functionality below is currently exercised via direct API calls.

- [x] Milestone 1 — Foundation (research, architecture, FSRS algorithm selection)
- [x] Milestone 2 — Core learning engine (FSRS scheduling wired to a live DB, AI question generation)
- [x] Milestone 3 — Socratic Tutor + Feynman Evaluation
- [ ] Leptos frontend
- [ ] User authentication (currently a known, documented limitation — see below)

---

## Tech Stack

- **Backend:** Rust, [Actix-web](https://actix.rs/)
- **Frontend (planned):** [Leptos](https://leptos.dev/) (full-stack Rust → WASM)
- **Database:** PostgreSQL via [Supabase](https://supabase.com/), accessed with [`sqlx`](https://github.com/launchbadge/sqlx)
- **Spaced repetition:** [`fsrs`](https://crates.io/crates/fsrs) crate (FSRS v6.6.1)
- **LLM:** [DeepSeek](https://www.deepseek.com/) (V4 Flash for cost-sensitive generation, V4 Pro for heavier reasoning)
- **Deployment (planned):** Vercel

---

## Architecture

See [`docs/architecture.md`](docs/architecture.md) for a full diagram distinguishing built-and-verified components from planned ones.

```
User → Leptos frontend (planned)
           ↓ HTTP
       Actix-web backend  ──→  mnemosyne-core (FSRS scheduling wrapper)
           ↓ sqlx                    ↓
      Supabase PostgreSQL      DeepSeek API (question gen / Socratic / Feynman)
```

---

## API Overview

| Endpoint | Purpose |
|---|---|
| `GET /health`, `GET /health/db` | Liveness + DB connectivity checks |
| `POST /users`, `GET /users` | User accounts |
| `POST /study_sets`, `GET /study_sets` | Study set (topic) management |
| `POST /cards`, `GET /cards` | Flashcard CRUD |
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
- A [Supabase](https://supabase.com/) project (free tier is sufficient)
- A [DeepSeek API](https://platform.deepseek.com/) key

### Setup

1. Clone the repo and copy the environment template:
   ```bash
   cp .env.example .env
   ```
2. Fill in `.env`:
   - `DATABASE_URL` — your Supabase **connection pooler** URI (not the direct connection — see [`docs/gotchas.md`](docs/gotchas.md) for why), with the password percent-encoded
   - `SUPABASE_URL`, `SUPABASE_ANON_KEY` — from your Supabase project's API settings
   - `DEEPSEEK_API_KEY` — from DeepSeek's platform
3. Apply the database schema: run `backend/sql/schema.sql` in the Supabase SQL Editor (this includes all tables from migrations 0001–0003; a fresh setup only needs this one file, not the individual migrations).
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
- [`docs/gotchas.md`](docs/gotchas.md) — infrastructure issues discovered during development (Supabase pooler/IPv6, sqlx prepared-statement collisions) and their fixes
- [`docs/case-study.md`](docs/case-study.md) — full research case study, including adversarial testing of the AI features (sycophancy detection, scoring-discrimination validation)

---

## Development Methodology

This project was built through a structured three-party collaboration: a human developer, an AI acting as technical planner/reviewer, and an AI coding agent ([OpenCode](https://opencode.ai)) doing implementation work. Every schema change was authored but never auto-executed — live database changes were applied only after human review. Every "this works" claim was required to include real, pasted command output rather than a natural-language summary. Details in [`docs/case-study.md`](docs/case-study.md#appendix-development-methodology-note).

---

## License

MIT
