-- =============================================================================
-- Migration 0002: Add Socratic dialogue session and message tables
-- =============================================================================
-- Milestone 3: Socratic Tutor.
--
-- Adds two new tables to support Socratic-method dialogue between a learner
-- and the DeepSeek AI tutor:
--
--   * socratic_sessions  -- one row per dialogue session, scoped to a study_set
--                           (the student is exploring the topic of that set as
--                           a whole, not being questioned on a single card)
--   * socratic_messages  -- individual turns within a session, ordered by
--                           created_at. role distinguishes user (the learner)
--                           from assistant (the AI tutor). flagged_misconception
--                           is populated only on assistant rows where the AI
--                           detected a misconception in the preceding user turn.
--
-- Cascade behavior (intentional): deleting a study_set cascades to delete its
-- socratic_sessions, which cascade to delete their socratic_messages. A deleted
-- study set should not leave orphaned conversation history.
--
-- This is a plain SQL authoring artifact. It has NOT been executed against any
-- live database. It is intended for manual review and application by a human
-- via the Supabase dashboard or CLI.
-- =============================================================================

-- ---------------------------------------------------------------------------
-- Table: socratic_sessions
-- One row per Socratic dialogue session. A session is scoped to a study_set
-- (the student is exploring/being questioned on that set's topic as a whole,
-- not a single card). Decided per product requirement, not a card-level scope.
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
-- Individual turns within a session, ordered by created_at. 'role'
-- distinguishes the student's messages from the AI tutor's.
-- 'flagged_misconception' is populated ONLY on assistant-role rows where the
-- AI detected a specific misconception in the student's preceding message;
-- NULL otherwise (including on all user-role rows, and on assistant-role rows
-- where no misconception was detected).
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