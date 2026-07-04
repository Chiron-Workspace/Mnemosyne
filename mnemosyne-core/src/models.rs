//! Domain types for Mnemosyne's scheduling layer.
//!
//! These types are intentionally decoupled from the `fsrs` crate's internal
//! types so that the rest of the application (backend API, future persistence
//! layer, tests) does not need to depend on `fsrs` directly. The
//! [`crate::scheduling`] module is the only place that translates between
//! these domain types and the `fsrs` crate's API.
//!
//! ## Mapping to the FSRS DSR model
//!
//! FSRS models each memory with three variables (see `docs/research.md`
//! §2.1 and the ADR at `docs/adr/0001-spaced-repetition-algorithm.md`):
//!
//! - **Stability (S):** days until recall probability drops from 100% to the
//!   target retention (default 90%). Stored as `f32` in [`CardState::stability`].
//! - **Difficulty (D):** inherent card hardness on a 1-10 scale, with mean
//!   reversion (unlike SM-2's ease factor). Stored as `f32` in
//!   [`CardState::difficulty`].
//! - **Retrievability (R):** current probability of recall, decays
//!   continuously over time. Not stored — computed on demand from
//!   stability + elapsed time via the forgetting curve.

use chrono::{DateTime, Utc};

/// The learner's self-assessment of a single review attempt.
///
/// Mirrors the four rating buttons exposed by FSRS (and Anki). The `fsrs`
/// crate itself uses `u32` values 1-4 internally (no public enum); this
/// enum provides a typed domain boundary and maps to/from those values in
/// [`crate::scheduling`].
///
/// Order is significant: `Again < Hard < Good < Easy` matches FSRS's
/// numeric ordering (1 < 2 < 3 < 4), so `as u32` is a valid FSRS rating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Rating {
    /// Card forgotten — reset stability, schedule again soon. FSRS rating 1.
    Again = 1,
    /// Correct but with significant effort. FSRS rating 2.
    Hard = 2,
    /// Correct with normal effort. FSRS rating 3.
    Good = 3,
    /// Correct with no effort. FSRS rating 4.
    Easy = 4,
}

/// The FSRS memory state for a single card, at a point in time.
///
/// This is the scheduling-level view of a card — not the full database row
/// (which also carries `card_id`, `user_id`, `response`, etc., handled by the
/// persistence layer). `stability` and `difficulty` are the two FSRS state
/// variables; `due` is the next scheduled review time and `last_review` is
/// when the card was last seen (needed to compute elapsed days).
#[derive(Debug, Clone, PartialEq)]
pub struct CardState {
    /// FSRS Stability: days until recall drops from 100% to target retention.
    pub stability: f32,
    /// FSRS Difficulty: card hardness 1-10, mean-reverting.
    pub difficulty: f32,
    /// When the card is next due for review.
    pub due: DateTime<Utc>,
    /// When the card was last reviewed. `None` for a brand-new card that has
    /// never been seen (in which case `stability`/`difficulty` are placeholders
    /// and the scheduler will initialize them on first review).
    pub last_review: Option<DateTime<Utc>>,
}

/// A single review event in the card's history.
///
/// Represents the outcome of one review attempt: what the learner rated,
/// when it happened, and the resulting [`CardState`] the scheduler produced.
/// This is the domain analogue of a row in the `learning_events` table
/// (see `backend/sql/schema.sql`), stripped of persistence concerns.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewLog {
    /// The rating the learner gave for this review.
    pub rating: Rating,
    /// When the review occurred.
    pub reviewed_at: DateTime<Utc>,
    /// The card state computed by the scheduler after applying `rating`.
    /// This is what should be persisted as the card's new current state.
    pub resulting_state: CardState,
}
