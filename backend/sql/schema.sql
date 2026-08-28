-- =============================================================================
-- Mnemosyne Database Schema
-- PostgreSQL DDL for the Mnemosyne personalized learning platform.
-- Assumes a PostgreSQL 14+ database with pgcrypto extension for UUID generation.
-- =============================================================================

-- Enable UUID generation
CREATE EXTENSION IF NOT EXISTS "pgcrypto";

-- ---------------------------------------------------------------------------
-- Table: users
-- Core user accounts. Each user has a unique email and optional learning style
-- preference that helps the AI adapt pedagogical approach.
-- ---------------------------------------------------------------------------
CREATE TABLE users (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    email           TEXT NOT NULL UNIQUE,
    learning_style  TEXT,  -- e.g., 'text', 'kinesthetic', 'visual', 'auditory'
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_users_email ON users (email);

-- ---------------------------------------------------------------------------
-- Table: study_sets
-- A collection of flashcards grouped by topic or subject area. Each set is
-- owned by exactly one user and can optionally be tagged with a topic for
-- organizational purposes.
-- ---------------------------------------------------------------------------
CREATE TABLE study_sets (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    topic       TEXT,  -- optional subject classification
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_study_sets_user_id ON study_sets (user_id);
CREATE INDEX idx_study_sets_topic ON study_sets (topic);

-- ---------------------------------------------------------------------------
-- Table: cards
-- Individual flashcards belonging to a study set. Each card contains a
-- question (prompt) and an answer (the target knowledge to be recalled).
-- ---------------------------------------------------------------------------
CREATE TABLE cards (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    set_id      UUID NOT NULL REFERENCES study_sets(id) ON DELETE CASCADE,
    question    TEXT NOT NULL,
    answer      TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_cards_set_id ON cards (set_id);

-- ---------------------------------------------------------------------------
-- Table: learning_events
-- Records every review attempt a user makes on a card. This is the primary
-- data table for the FSRS spaced repetition algorithm. Each event captures
-- the user's response, correctness, and the resulting scheduler state
-- (stability, difficulty, interval, next review date).
--
-- FSRS state columns (stability, difficulty) were added by migration
-- 0001_add_fsrs_fields.sql following ADR 0001. The legacy ease_factor column
-- is retained but unused post-FSRS-adoption; see migration 0001 for details.
-- ---------------------------------------------------------------------------
CREATE TABLE learning_events (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    card_id         UUID NOT NULL REFERENCES cards(id) ON DELETE CASCADE,
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    response        TEXT,  -- the user's free-form answer, if provided
    is_correct      BOOLEAN NOT NULL,
    ease_factor     FLOAT NOT NULL DEFAULT 2.5,  -- LEGACY/UNUSED post-FSRS (see ADR 0001 + migration 0001); FSRS uses stability + difficulty instead
    stability       FLOAT,  -- FSRS: days until recall drops from 100% to 90% (nullable for pre-FSRS rows)
    difficulty       FLOAT,  -- FSRS: inherent card hardness 1-10, mean-reverting (nullable for pre-FSRS rows)
    interval        INTEGER NOT NULL DEFAULT 0,  -- days until next review
    next_review_at  TIMESTAMPTZ NOT NULL,  -- when this card should next be reviewed
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_learning_events_card_id ON learning_events (card_id);
CREATE INDEX idx_learning_events_user_id ON learning_events (user_id);
CREATE INDEX idx_learning_events_next_review_at ON learning_events (next_review_at);
CREATE INDEX idx_learning_events_user_card ON learning_events (user_id, card_id);

-- ---------------------------------------------------------------------------
-- Table: ai_interactions
-- Logs all exchanges between the user and the DeepSeek AI tutor. Used for
-- cost tracking, quality monitoring, and improving the AI's pedagogical
-- effectiveness over time. The interaction_type enum classifies the
-- pedagogical purpose of each exchange.
-- ---------------------------------------------------------------------------

-- Define the interaction type enum
CREATE TYPE ai_interaction_type AS ENUM (
    'question_generation',
    'socratic_dialogue',
    'feynman_evaluation',
    'quiz_generation'
);

CREATE TABLE ai_interactions (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id             UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    interaction_type    ai_interaction_type NOT NULL,
    input_text          TEXT NOT NULL,   -- the user's message to the AI
    output_text         TEXT NOT NULL,   -- the AI's response
    tokens_used         INTEGER NOT NULL DEFAULT 0,  -- LLM token count for cost tracking
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_ai_interactions_user_id ON ai_interactions (user_id);
CREATE INDEX idx_ai_interactions_type ON ai_interactions (interaction_type);
CREATE INDEX idx_ai_interactions_created_at ON ai_interactions (created_at);

-- ---------------------------------------------------------------------------
-- Table: socratic_sessions
-- One row per Socratic dialogue session. A session is scoped to a study_set
-- (the student is exploring/being questioned on that set's topic as a whole,
-- not a single card). Added by migration 0002_add_socratic_tables.sql.
-- ---------------------------------------------------------------------------
CREATE TABLE socratic_sessions (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    set_id      UUID NOT NULL REFERENCES study_sets(id) ON DELETE CASCADE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_socratic_sessions_user_id ON socratic_sessions (user_id);
CREATE INDEX idx_socratic_sessions_set_id ON socratic_sessions (set_id);

-- ---------------------------------------------------------------------------
-- Table: socratic_messages
-- Individual turns within a session, ordered by created_at. 'role' distinguishes
-- the student's messages from the AI tutor's. 'flagged_misconception' is
-- populated ONLY on assistant-role rows where the AI detected a specific
-- misconception in the student's preceding message; NULL otherwise (including
-- on all user-role rows, and on assistant-role rows where no misconception was
-- detected). Added by migration 0002_add_socratic_tables.sql.
-- ---------------------------------------------------------------------------
CREATE TABLE socratic_messages (
    id                      UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    session_id              UUID NOT NULL REFERENCES socratic_sessions(id) ON DELETE CASCADE,
    role                    TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    content                 TEXT NOT NULL,
    flagged_misconception   TEXT,
    created_at              TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_socratic_messages_session_id ON socratic_messages (session_id);
CREATE INDEX idx_socratic_messages_session_created ON socratic_messages (session_id, created_at);

-- ---------------------------------------------------------------------------
-- Table: feynman_evaluations
-- One row per Feynman Technique submission: a student writes a free-text
-- explanation of a study_set's topic in their own words, and the AI evaluates
-- it on three dimensions (clarity, completeness, correctness), each scored
-- 1-10, plus free-text feedback and improvement suggestions. Scoped to a
-- study_set (the student explains the topic as a whole), not a single card.
-- Added by migration 0003_add_feynman_evaluations.sql.
--
-- Storing structured scores (not just a log entry) is intentional: this
-- supports tracking a user's explanation quality over time for the same
-- study_set, which is a planned evaluation metric (see docs/research.md §5).
-- ---------------------------------------------------------------------------
CREATE TABLE feynman_evaluations (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id              UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    set_id               UUID NOT NULL REFERENCES study_sets(id) ON DELETE CASCADE,
    explanation_text     TEXT NOT NULL,          -- the student's own-words explanation
    clarity_score        INTEGER NOT NULL CHECK (clarity_score BETWEEN 1 AND 10),
    completeness_score   INTEGER NOT NULL CHECK (completeness_score BETWEEN 1 AND 10),
    correctness_score    INTEGER NOT NULL CHECK (correctness_score BETWEEN 1 AND 10),
    feedback             TEXT NOT NULL,          -- overall AI feedback paragraph
    suggestions          TEXT NOT NULL,          -- specific improvement suggestions
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_feynman_evaluations_user_id ON feynman_evaluations (user_id);
CREATE INDEX idx_feynman_evaluations_set_id ON feynman_evaluations (set_id);
CREATE INDEX idx_feynman_evaluations_user_set ON feynman_evaluations (user_id, set_id, created_at);

-- ---------------------------------------------------------------------------
-- Table: quiz_questions
-- Multiple-choice questions generated by an LLM. Added by migration
-- 0004_add_quiz_tables.sql.
--
-- Quiz is the one methodology whose grading needs no model: the answer is an
-- index, so checking it is a comparison. Socratic is a dialogue and Feynman is
-- free text scored by the LLM; a quiz answer is objectively checkable, and
-- grading must not cost a token or vary between runs.
--
-- `source` records provenance: 'topic' for questions generated from free text
-- the user supplied, 'knowledge_store' for questions generated from a concept
-- node already in the Chiron Knowledge Store. There is deliberately no foreign
-- key on source_node_id — the Knowledge Store is a separate database
-- (chiron_ks) on the same cluster, and Postgres cannot enforce referential
-- integrity across databases. The column is a traceability breadcrumb.
-- ---------------------------------------------------------------------------
CREATE TABLE quiz_questions (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    set_id          UUID NOT NULL REFERENCES study_sets(id) ON DELETE CASCADE,
    question        TEXT NOT NULL,
    choices         JSONB NOT NULL,   -- array of strings, one per option
    correct_index   INTEGER NOT NULL, -- 0-based index into choices
    source          TEXT NOT NULL CHECK (source IN ('topic', 'knowledge_store')),
    source_node_id  UUID,             -- ks.nodes(id) when source='knowledge_store'; no FK, different database
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT quiz_questions_choices_is_array
        CHECK (jsonb_typeof(choices) = 'array'),
    CONSTRAINT quiz_questions_choices_min_length
        CHECK (jsonb_array_length(choices) >= 2),
    CONSTRAINT quiz_questions_correct_index_in_range
        CHECK (correct_index >= 0 AND correct_index < jsonb_array_length(choices)),
    CONSTRAINT quiz_questions_node_id_iff_knowledge_store
        CHECK ((source = 'knowledge_store') = (source_node_id IS NOT NULL))
);

CREATE INDEX idx_quiz_questions_set_id ON quiz_questions (set_id);
CREATE INDEX idx_quiz_questions_source ON quiz_questions (source);

-- ---------------------------------------------------------------------------
-- Table: quiz_attempts
-- One row per submitted answer. `is_correct` is stored rather than derived on
-- read: it is computed once at answer time from the question as it existed
-- then, so later edits to a question cannot retroactively rewrite a learner's
-- history. Added by migration 0004_add_quiz_tables.sql.
-- ---------------------------------------------------------------------------
CREATE TABLE quiz_attempts (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    question_id     UUID NOT NULL REFERENCES quiz_questions(id) ON DELETE CASCADE,
    user_id         UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    selected_index  INTEGER NOT NULL CHECK (selected_index >= 0),
    is_correct      BOOLEAN NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_quiz_attempts_question_id ON quiz_attempts (question_id);
CREATE INDEX idx_quiz_attempts_user_id ON quiz_attempts (user_id);
CREATE INDEX idx_quiz_attempts_user_created ON quiz_attempts (user_id, created_at);
