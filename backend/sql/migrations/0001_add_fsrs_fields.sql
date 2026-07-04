-- =============================================================================
-- Migration 0001: Add FSRS state columns to learning_events
-- =============================================================================
-- References: docs/adr/0001-spaced-repetition-algorithm.md
--
-- ADR 0001 decided to adopt FSRS (Free Spaced Repetition Scheduler) as the
-- scheduling algorithm for Mnemosyne. FSRS models per-card memory state using
-- two core variables that the original learning_events table (authored in
-- Prompt 1, pre-ADR) does not store:
--
--   * stability  -- days until recall probability drops from 100% to 90%
--   * difficulty -- inherent card hardness on a 1-10 scale (mean-reverting)
--
-- This migration adds both columns (nullable, since historical rows predating
-- FSRS adoption will not have these values populated).
--
-- Decision on ease_factor: KEPT for now. It is legacy/unused post-FSRS-adoption
-- (FSRS does not use an ease factor; it uses stability + difficulty instead).
-- A SQL comment is added below to flag it. The column is NOT dropped in this
-- migration -- dropping is a separate, more careful decision deferred to a
-- future cleanup migration once FSRS integration is verified in production.
--
-- This file is a plain SQL authoring artifact. It has NOT been executed
-- against any live database. It is intended for manual review and application
-- by a human via the Supabase dashboard or CLI.
-- =============================================================================

-- Add FSRS stability column (nullable: existing rows have no FSRS state).
ALTER TABLE learning_events ADD COLUMN stability FLOAT;

-- Add FSRS difficulty column (nullable: existing rows have no FSRS state).
ALTER TABLE learning_events ADD COLUMN difficulty FLOAT;

-- Flag ease_factor as legacy. PostgreSQL column comments are queryable via
-- pg_catalog and surface in tooling like Supabase's table editor.
COMMENT ON COLUMN learning_events.ease_factor IS
    'LEGACY/UNUSED post-FSRS adoption (see ADR 0001). Retained for backward compatibility; FSRS uses stability + difficulty instead. Removal deferred to a future cleanup migration.';
