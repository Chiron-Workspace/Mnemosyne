//! Thin wrapper around the `fsrs` crate exposing Mnemosyne's domain API.
//!
//! This module is the **only** place in the codebase that knows about the
//! `fsrs` crate's concrete types (`fsrs::FSRS`, `fsrs::MemoryState`,
//! `fsrs::NextStates`, `fsrs::ItemState`). Everything else in Mnemosyne
//! talks in terms of [`crate::models`] types. This boundary keeps the rest
//! of the app insulated from upstream `fsrs` API changes.
//!
//! ## Why a struct, not a trait?
//!
//! `FsrsScheduler` is a concrete struct rather than a trait. Rationale:
//! (1) only one scheduling implementation exists and is planned (FSRS), so a
//! trait would be premature abstraction with no second implementation to
//! justify it; (2) the struct carries real state (the `fsrs::FSRS` instance
//! and configured `desired_retention`), which a trait would still need to
//! pass through. If a second algorithm or a mock-for-tests is needed later,
//! a trait can be extracted at that point without breaking callers.

use chrono::{DateTime, Duration, Utc};
use fsrs::{FSRS, FSRS6_DEFAULT_DECAY, ItemState, MemoryState, NextStates, current_retrievability};

use crate::models::{CardState, Rating, ReviewLog};

/// Default target retention (probability of recall at next review).
///
/// 0.9 is FSRS's own default and matches what Anki uses out of the box.
const DEFAULT_DESIRED_RETENTION: f32 = 0.9;

/// Errors produced by the scheduling layer.
#[derive(Debug)]
pub enum SchedulerError {
    /// The `fsrs` crate rejected the inputs (e.g. invalid desired retention).
    Fsrs(fsrs::FSRSError),
}

impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchedulerError::Fsrs(e) => write!(f, "fsrs scheduling failed: {e}"),
        }
    }
}

impl std::error::Error for SchedulerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SchedulerError::Fsrs(e) => Some(e),
        }
    }
}

impl From<fsrs::FSRSError> for SchedulerError {
    fn from(e: fsrs::FSRSError) -> Self {
        SchedulerError::Fsrs(e)
    }
}

/// Result of scheduling a brand-new (never-reviewed) card: the four possible
/// next states corresponding to each [`Rating`], before the learner has
/// picked one.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCardStates {
    pub again: CardState,
    pub hard: CardState,
    pub good: CardState,
    pub easy: CardState,
}

/// Thin wrapper around `fsrs::FSRS` exposing Mnemosyne domain types.
///
/// Construct once (cheap — clones a 21-element parameter array), reuse for
/// many cards. `desired_retention` is fixed at construction; per-card
/// retention targets are a future feature, not needed for the 2-3 user
/// initial deployment (see `docs/spaced-rep-spike.md` §5).
pub struct FsrsScheduler {
    fsrs: FSRS,
    desired_retention: f32,
}

impl Default for FsrsScheduler {
    fn default() -> Self {
        Self {
            fsrs: FSRS::default(),
            desired_retention: DEFAULT_DESIRED_RETENTION,
        }
    }
}

impl FsrsScheduler {
    /// Construct with default FSRS parameters and an explicit desired retention.
    ///
    /// `desired_retention` should be in (0.0, 1.0); values outside this range
    /// will be rejected by the `fsrs` crate at scheduling time.
    pub fn new(desired_retention: f32) -> Self {
        Self {
            fsrs: FSRS::default(),
            desired_retention,
        }
    }

    /// Schedule a brand-new card that has never been reviewed.
    ///
    /// Returns the four possible next states (one per [`Rating`]); the caller
    /// picks the one matching the learner's actual rating and persists it as
    /// the card's new [`CardState`].
    ///
    /// `now` is passed explicitly rather than read from the system clock so
    /// tests are deterministic.
    pub fn schedule_new(&self, now: DateTime<Utc>) -> Result<NewCardStates, SchedulerError> {
        let next = self.fsrs.next_states(None, self.desired_retention, 0)?;
        Ok(map_next_states(next, now, now))
    }

    /// Schedule an existing card given its current state and a new rating.
    ///
    /// `elapsed_days` is derived from `current.last_review` and `now`; if
    /// `last_review` is `None` (card state exists but last review timestamp
    /// was lost — shouldn't normally happen), 0 elapsed days is assumed and
    /// the card is treated as just-seen.
    ///
    /// Returns the updated [`CardState`] (with new stability/difficulty/due)
    /// and a [`ReviewLog`] recording the event.
    pub fn schedule_review(
        &self,
        current: &CardState,
        rating: Rating,
        now: DateTime<Utc>,
    ) -> Result<(CardState, ReviewLog), SchedulerError> {
        let elapsed_days = elapsed_days(current.last_review, now);

        // We only need the one rating the learner picked, but `next_states`
        // returns all four (the fsrs crate offers no single-rating API).
        // Compute all four, then select. Cheaper than reimplementing the math
        // and stays correct if fsrs internals change.
        let next = self.fsrs.next_states(
            Some(MemoryState {
                stability: current.stability,
                difficulty: current.difficulty,
            }),
            self.desired_retention,
            elapsed_days,
        )?;

        let picked = pick_state(next, rating);
        let new_state = CardState {
            stability: picked.memory.stability,
            difficulty: picked.memory.difficulty,
            due: now + Duration::days(picked.interval.max(1.0).round() as i64),
            last_review: Some(now),
        };
        let log = ReviewLog {
            rating,
            reviewed_at: now,
            resulting_state: new_state.clone(),
        };
        Ok((new_state, log))
    }

    /// Compute current retrievability (probability of recall) for a card.
    ///
    /// Returns a value in [0.0, 1.0]. Uses the FSRS power-law forgetting
    /// curve with the default FSRS-6 decay parameter.
    pub fn retrievability(&self, state: &CardState, now: DateTime<Utc>) -> f32 {
        let elapsed = elapsed_days(state.last_review, now) as f32;
        current_retrievability(
            MemoryState {
                stability: state.stability,
                difficulty: state.difficulty,
            },
            elapsed,
            FSRS6_DEFAULT_DECAY,
        )
    }

    /// Configured target retention for this scheduler.
    pub fn desired_retention(&self) -> f32 {
        self.desired_retention
    }
}

/// Map the `fsrs::NextStates` (all four ratings) into domain `CardState`s.
///
/// For a new card there is no `last_review`, so `due` is `now + interval`
/// and `last_review` is set to `now` (the card is being seen right now).
fn map_next_states(next: NextStates, now: DateTime<Utc>, _last: DateTime<Utc>) -> NewCardStates {
    NewCardStates {
        again: item_to_card_state(next.again, now),
        hard: item_to_card_state(next.hard, now),
        good: item_to_card_state(next.good, now),
        easy: item_to_card_state(next.easy, now),
    }
}

/// Convert one `fsrs::ItemState` into a domain `CardState`.
fn item_to_card_state(item: ItemState, now: DateTime<Utc>) -> CardState {
    CardState {
        stability: item.memory.stability,
        difficulty: item.memory.difficulty,
        due: now + Duration::days(item.interval.max(1.0).round() as i64),
        last_review: Some(now),
    }
}

/// Select the `ItemState` from a `NextStates` matching the given rating.
fn pick_state(next: NextStates, rating: Rating) -> ItemState {
    match rating {
        Rating::Again => next.again,
        Rating::Hard => next.hard,
        Rating::Good => next.good,
        Rating::Easy => next.easy,
    }
}

/// Whole days between `last_review` (if any) and `now`, clamped to >= 0.
fn elapsed_days(last_review: Option<DateTime<Utc>>, now: DateTime<Utc>) -> u32 {
    match last_review {
        Some(last) => (now - last).num_days().max(0) as u32,
        None => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// Fixed reference time for deterministic tests.
    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 4, 12, 0, 0).unwrap()
    }

    // -------------------------------------------------------------------------
    // TEST GROUP 1: new-card scheduling, one assertion per rating
    // -------------------------------------------------------------------------

    #[test]
    fn new_card_easy_has_higher_stability_than_again() {
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        // Easy (rating 4) initializes stability from DEFAULT_PARAMETERS[3]
        // Again (rating 1) initializes stability from DEFAULT_PARAMETERS[0]
        // Default params: [0] = 0.212, [3] = 8.2956 — Easy should be much higher.
        assert!(
            states.easy.stability > states.again.stability,
            "Easy stability ({}) should exceed Again stability ({})",
            states.easy.stability,
            states.again.stability
        );
    }

    #[test]
    fn new_card_stability_monotonic_across_ratings() {
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        // Stability should increase monotonically: Again < Hard < Good < Easy.
        assert!(states.again.stability < states.hard.stability);
        assert!(states.hard.stability < states.good.stability);
        assert!(states.good.stability < states.easy.stability);
    }

    #[test]
    fn new_card_difficulty_decreases_across_ratings() {
        // Harder rating (Again) → higher difficulty. Easier rating (Easy)
        // → lower difficulty. This matches init_difficulty() in fsrs model.rs.
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        assert!(states.again.difficulty > states.hard.difficulty);
        assert!(states.hard.difficulty > states.good.difficulty);
        assert!(states.good.difficulty > states.easy.difficulty);
    }

    #[test]
    fn new_card_intervals_are_at_least_one_day() {
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        for (name, state) in [
            ("again", &states.again),
            ("hard", &states.hard),
            ("good", &states.good),
            ("easy", &states.easy),
        ] {
            let days = (state.due - t0()).num_days();
            assert!(
                days >= 1,
                "new-card {} interval {} should be >= 1 day",
                name,
                days
            );
        }
    }

    // -------------------------------------------------------------------------
    // TEST GROUP 2: multi-review sequence, interval growth
    // -------------------------------------------------------------------------

    #[test]
    fn repeated_good_ratings_grow_interval() {
        // A card reviewed Good 4 times in a row should have monotonically
        // growing intervals (each review scheduled further out than the last).
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        let mut state = states.good.clone();

        let mut intervals = vec![(state.due - t0()).num_days()];
        for _ in 1..=3 {
            // Advance time to the card's due date before reviewing.
            let now = state.due;
            let (next, _log) = s.schedule_review(&state, Rating::Good, now).unwrap();
            let interval = (next.due - now).num_days();
            intervals.push(interval);
            state = next;
        }

        // Intervals: [initial_good, then 3 more]. Each should exceed the prior.
        for w in intervals.windows(2) {
            assert!(
                w[1] > w[0],
                "interval should grow monotonically under Good ratings: {} -> {}",
                w[0],
                w[1]
            );
        }
        // Sanity: after 4 Good reviews, interval should be at least a week.
        assert!(
            *intervals.last().unwrap() >= 7,
            "interval after 4 Good ratings should be >= 7 days, got {}",
            intervals.last().unwrap()
        );
    }

    #[test]
    fn again_resets_or_shrinks_interval() {
        // A card rated Again should produce a short next interval (<= a few
        // days), reflecting that the card was forgotten.
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        // Start from a Good initial state (so the card has real stability),
        // then rate Again.
        let (after_again, _log) = s
            .schedule_review(&states.good, Rating::Again, t0())
            .unwrap();
        let interval_days = (after_again.due - t0()).num_days();
        assert!(
            interval_days <= 3,
            "Again should produce a short interval (<= 3 days), got {}",
            interval_days
        );
    }

    // -------------------------------------------------------------------------
    // TEST GROUP 3: cross-check against fsrs crate's own primitives
    // (validates the wrapper translates correctly, not just self-consistency)
    // -------------------------------------------------------------------------

    #[test]
    fn wrapper_stability_matches_fsrs_default_parameters_for_new_card() {
        // The fsrs crate's DEFAULT_PARAMETERS (inference.rs:21) defines
        // initial stability per rating at indices [0..4]. Our wrapper should
        // produce exactly these values for a brand-new card.
        let expected = fsrs::DEFAULT_PARAMETERS;
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        assert_eq!(states.again.stability, expected[0]);
        assert_eq!(states.hard.stability, expected[1]);
        assert_eq!(states.good.stability, expected[2]);
        assert_eq!(states.easy.stability, expected[3]);
    }

    #[test]
    fn retrievability_is_1_at_zero_elapsed_days() {
        // A card just reviewed has 100% retrievability.
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        let r = s.retrievability(&states.good, t0());
        assert!(
            r > 0.99,
            "retrievability at 0 elapsed days should be ~1.0, got {}",
            r
        );
    }

    #[test]
    fn retrievability_decreases_over_time() {
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        let r_now = s.retrievability(&states.good, t0());
        let r_later = s.retrievability(&states.good, t0() + Duration::days(30));
        assert!(
            r_later < r_now,
            "retrievability should decrease over 30 days: {} -> {}",
            r_now,
            r_later
        );
    }

    #[test]
    fn review_log_records_rating_and_timestamp() {
        let s = FsrsScheduler::default();
        let states = s.schedule_new(t0()).unwrap();
        let (_new, log) = s
            .schedule_review(&states.good, Rating::Hard, t0())
            .unwrap();
        assert_eq!(log.rating, Rating::Hard);
        assert_eq!(log.reviewed_at, t0());
    }
}
