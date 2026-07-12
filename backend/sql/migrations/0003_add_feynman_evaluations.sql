-- =============================================================================
-- Migration 0003: Add Feynman Technique evaluation table
-- =============================================================================
-- Milestone 3: Feynman Technique evaluation.
--
-- Adds one new table to support the Feynman Technique learning methodology:
-- a student writes a free-text explanation of a study_set's topic in their
-- own words, and the DeepSeek AI evaluates the explanation on three
-- dimensions (clarity, completeness, correctness), each scored 1-10, plus
-- free-text feedback and improvement suggestions.
--
-- Scoped to a study_set (the student explains the topic as a whole), not a
-- single card — consistent with how Socratic sessions are also study_set-
-- scoped.
--
-- Storing structured scores (not just a log entry in ai_interactions) is
-- intentional: this supports tracking a user's explanation quality over time
-- for the same study_set, which is a planned evaluation metric for the
-- project's research angle (see docs/research.md §5, "Evaluation Metrics").
--
-- Cascade behavior (intentional): deleting a user or study_set cascades to
-- delete their feynman_evaluations. A deleted user/study_set should not
-- leave orphaned evaluation history.
--
-- This is a plain SQL authoring artifact. It has NOT been executed against
-- any live database. It is intended for manual review and application by a
-- human via the Supabase dashboard or CLI.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- Table: feynman_evaluations
-- One row per Feynman Technique submission: a student writes a free-text
-- explanation of a study_set's topic in their own words, and the AI evaluates
-- it on three dimensions (clarity, completeness, correctness), each scored
-- 1-10, plus free-text feedback and improvement suggestions. Scoped to a
-- study_set (the student explains the topic as a whole), not a single card
-- -- decided per product requirement, consistent with how Socratic sessions
-- are also study_set-scoped.
--
-- Storing structured scores (not just a log entry) is intentional: this
-- supports tracking a user's explanation quality over time for the same
-- study_set, which is a planned evaluation metric for the project's research
-- angle (see docs/research.md section 5).
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