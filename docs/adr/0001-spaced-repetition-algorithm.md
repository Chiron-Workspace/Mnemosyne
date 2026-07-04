# ADR 0001: Spaced Repetition Algorithm Choice

## Status
Proposed

## Context
Mnemosyne needs a core scheduling algorithm to determine when to show flashcards for review. The options are SM-2 (1987, used by Anki until 2023) and FSRS (2022-2026, modern Anki default). The srs-benchmark on 9,999 users and ~350M reviews shows FSRS achieves 20-30% fewer reviews for the same retention, with 99.6% of users seeing better recall predictions. Both have mature Rust crates under the `open-spaced-repetition` GitHub org.

## Decision
We will use FSRS via the `fsrs` Rust crate (v6.6.1) as the sole spaced repetition scheduler.

## Data
- **Benchmark accuracy:** FSRS-6 log loss = 0.346; FSRS-rs (Rust port) log loss = 0.344; SM-2 (adapted) estimated at ~0.43+ (Expertium benchmark, 2024)
- **Review efficiency:** FSRS requires 20-30% fewer reviews to maintain the same retention rate (simulation data from open-spaced-repetition benchmark)
- **Cold start:** FSRS default parameters were trained on ~700M reviews and outperform SM-2 on 99.6% of users without any per-user optimization
- **Rust crate health:** `fsrs` crate has 323k+ total downloads, 73 versions, latest v6.6.1 (June 2026), MIT license, maintained by the same org that runs the benchmark
- **Industry adoption in 2026:** Anki (default), RemNote, Atomus, Flica, and most new SRS apps all ship FSRS

## Consequences
- **Schema change needed from Prompt 1:** The `learning_events` table needs two new columns — `stability` (FLOAT) and `difficulty` (FLOAT) — to store FSRS per-card state. The existing `ease_factor` column can be retained for backward compatibility or dropped. The `interval` and `next_review_at` columns remain the same (FSRS outputs both).
- **Implementation complexity:** Higher than SM-2, but the `fsrs` crate abstracts all complexity. Mnemosyne's wrapper layer is estimated at 100-200 lines of glue code (mapping Mnemosyne review events to FSRS `ReviewLog` structs and calling `scheduler.next_review()`).
- **Loss of simplicity:** SM-2's state is trivially debuggable (just EF + interval). FSRS state (stability + difficulty + 21 global params) is harder to inspect manually. This is acceptable for a portfolio project where the learning value of FSRS is itself a feature.
- **No SM-2 fallback:** We are not implementing SM-2 at all. If the `fsrs` crate has a critical bug, we would need to patch or replace it. The crate's maturity and Anki production usage make this risk extremely low.

## Follow-up
1. Schema migration: add `stability` and `difficulty` columns to `learning_events`
2. Add `fsrs = "6.6.1"` to Cargo.toml in the backend crate
3. Implement a `SpacedRepScheduler` trait with FSRS implementation
4. Write unit tests using known test vectors from the FSRS repository
5. Test cold-start behavior with 0 prior reviews (should produce sensible default intervals)
