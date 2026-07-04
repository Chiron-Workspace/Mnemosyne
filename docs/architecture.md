# Mnemosyne System Architecture

> **Status:** Milestone 1 — infrastructure skeleton and FSRS scheduling implemented.
> Solid lines indicate existing, tested components; dashed lines indicate planned/future components.

```mermaid
flowchart TB
    subgraph Client["Client"]
        USER([User / Learner])
        LEPTOS[Leptos Frontend<br/>(WASM in browser)]
    end

    subgraph Backend["Backend (Rust server)"]
        HTTP[Actix-web HTTP Server<br/>backend crate]
        CORE[mnemosyne-core<br/>FSRS Scheduling Wrapper]
    end

    subgraph Data["Data Layer"]
        SUPABASE[(Supabase PostgreSQL<br/>via sqlx)]
        DEEPSEEK{{DeepSeek API<br/>LLM Service}}
    end

    subgraph Integration["AI Integration (Future)"]
        QGEN[Question Generation]
        SOCRATIC[Socratic Dialogue]
        FEYNMAN[Feynman Evaluation]
    end

    USER -->|"click/type input"| LEPTOS
    LEPTOS -.-|"HTTP (future: plan only)"| HTTP

    HTTP -->|"schedule_review() / schedule_new()"| CORE
    CORE -->|"CardState / ReviewLog"| HTTP

    HTTP -.-|"SQL queries (future: sqlx)"| SUPABASE
    HTTP -.-|"POST /v1/chat/completions<br/>(future)"| DEEPSEEK

    DEEPSEEK -.- QGEN
    DEEPSEEK -.- SOCRATIC
    DEEPSEEK -.- FEYNMAN

    QGEN -.- HTTP
    SOCRATIC -.- HTTP
    FEYNMAN -.- HTTP

    classDef existing fill:#4ade80,stroke:#166534,color:#052e16
    classDef planned fill:#fcd34d,stroke:#92400e,color:#451a03
    classDef user fill:#93c5fd,stroke:#1e40af,color:#1e3a5f

    class HTTP,CORE existing
    class LEPTOS,SUPABASE,DEEPSEEK,QGEN,SOCRATIC,FEYNMAN planned
    class USER user
```

## Data Flow: Flashcard Review

Below is a description of the intended end-to-end path for a user reviewing a flashcard, from the moment they rate it to the updated schedule being persisted. **Note: only the FSRS scheduling step is implemented as of Milestone 1.** The frontend, database persistence, and AI integration are planned for later milestones.

1. **User rates a card** — The user clicks a rating button (Again/Hard/Good/Easy) in the Leptos frontend. This sends an HTTP `POST /review` request to the Actix-web backend with the `card_id` and the chosen `rating`.

2. **Backend calls FSRS** — The handler loads the card's current `CardState` (via `sqlx` from Supabase, once wired) and calls `FsrsScheduler::schedule_review()`, which translates the rating into an FSRS call and returns the updated `CardState` (new stability, difficulty, and due date) plus a `ReviewLog`.

3. **Persistence** — The new `CardState` is written back to Supabase (update the card record, insert the `learning_events` row). The backend then returns the updated card schedule (next review date, retrievability) to the frontend.

4. **AI enrichment (future)** — On certain triggers (e.g., the card was rated Again, or it has been reviewed N times), the backend optionally calls DeepSeek for Socratic probing or Feynman evaluation, and the AI response is displayed alongside the card's answer.
