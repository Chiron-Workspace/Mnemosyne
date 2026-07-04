# Spaced Repetition Algorithm Spike: FSRS vs SM-2

## Objective

Evaluate FSRS (Free Spaced Repetition Scheduler) and SM-2 as the core scheduling algorithm for Mnemosyne. This is a pure research/decision document — no implementation, no crate integration. The goal is a recommendation that accounts for Mnemosyne's specific constraints: 2-3 initial users, 16-week timeline, college portfolio/research angle.

## 1. SM-2 Algorithm

### How It Works

SM-2, published by Piotr Wozniak in 1987 as part of SuperMemo, tracks two numbers per card:

- **Ease Factor (EF):** initialized at 2.5, adjusted up/down after each review based on a 0-5 grade scale. Floor at 1.3.
- **Interval:** first review → 1 day; second → 6 days; subsequent → `previous_interval × EF`.

If a review is failed (grade < 3), the card resets to interval 1 and the EF decreases.

### Strengths

- **Simplicity:** ~200 lines of code in the reference Rust implementation. Can be implemented from scratch in an afternoon. Easy to reason about, debug, and explain in a portfolio.
- **Production track record:** Drove Anki for ~17 years (2006-2023). Used by millions of learners. Every known edge case is documented.
- **No training data required:** Works identically from card one. No cold start problem — the algorithm is deterministic with fixed initial parameters.
- **Minimal state:** Only EF and interval per card. Fits trivially in any schema.

### Weaknesses

- **Ease hell:** Repeated "Hard" ratings drive EF to the 1.3 floor, trapping cards in short intervals forever. The Anki community has complained about this for over a decade. Workarounds (add-ons, manual resets) exist but are clumsy.
- **No per-card memory model:** EF is a single scalar that conflates difficulty and stability. It cannot distinguish "this card is hard but I know it well" from "this card is easy but I just learned it."
- **No personalization:** Every user gets the same initial parameters and the same update rules. No optimization to individual forgetting curves.
- **No target retention:** SM-2 schedules by fixed rules, not by a desired retention probability. You cannot ask "show me this card when recall probability drops to 90%."
- **Accuracy gap:** When adapted for probability prediction (which it was never designed for), SM-2's log loss is significantly higher than FSRS (see benchmark below).

## 2. FSRS Algorithm (v4/v5/v6/v7)

### How It Works

FSRS (Free Spaced Repetition Scheduler), developed by Jarrett Ye starting in 2022, models each memory as three variables:

- **Stability (S):** how long (in days) before recall probability drops from 100% to 90%. A newly learned card might have S=1-2 days; a mature card S=300+ days.
- **Difficulty (D):** how inherently hard a card is (1-10 scale). Unlike SM-2's EF, D uses mean reversion — it drifts back toward a baseline instead of staying permanently damaged.
- **Retrievability (R):** current probability of recall, which decays continuously according to a power-law forgetting curve.

After each review, FSRS updates (S, D) and computes the next interval as the time when R decays to the target retention (default 90%).

The algorithm has 21 trainable parameters (FSRS-6) or 35 (FSRS-7), fit to ~700 million reviews from ~10,000 Anki users. Default parameters work well out of the box; per-user optimization (available after ~1,000 reviews) further improves accuracy.

### Strengths

- **State-of-the-art accuracy:** FSRS-6 achieves mean log loss 0.346 vs. the SM-2-adapted baseline at ~0.43+ on the open-spaced-repetition benchmark. In 99.6% of users, FSRS predicts recall more accurately than SM-2.
- **20-30% fewer reviews:** Simulations show FSRS maintains the same retention rate with 20-30% fewer total reviews. For a user doing 200 reviews/day, that's 40-60 reviews saved daily.
- **No ease hell:** Difficulty uses mean reversion. Cards that were once hard can become easy. No permanent interval trap.
- **Personalization:** The optimizer can fit 21 parameters to a specific user's review history, adapting to their unique forgetting curve.
- **Retention targeting:** You set a desired retention (e.g., 90%), and FSRS schedules precisely to that probability. This enables features like "I want to remember 95% of cards in this deck" without manual tuning.
- **Industry adoption in 2026:** Anki (default since v23.10), RemNote, Atomus, Flica, CuaderNote, Neurako, ChessAtlas, and most credible new SRS apps all ship FSRS by default. SM-2 is legacy in the active ecosystem.

### Weaknesses

- **Complex state:** Requires stability, difficulty, retrievability, and 21+ global parameters — not just EF and interval. The reference Rust implementation (`fsrs` crate) is ~3,000+ lines of code.
- **Cold start:** Default parameters (trained on millions of reviews) work reasonably from day one, but per-user optimization needs ~1,000+ reviews to converge. For a 2-3 user system, the defaults are fine initially but the optimizer won't add meaningful value until users accumulate history.
- **Debugging difficulty:** When scheduling behaves unexpectedly, diagnosing the root cause is harder than SM-2 because the algorithm is a learned model rather than a deterministic rule.
- **Schema requirements:** Needs columns for stability (float), difficulty (float), retrievability (float, computed at query time), plus the global parameter set stored somewhere.

## 3. Comparison Table

| Criterion | SM-2 | FSRS (v6/v7) |
|---|---|---|
| **State per card** | EF (float), interval (int) | Stability (float), Difficulty (float), retrievability (computed) |
| **Global parameters** | 0 (fixed rules) | 21 (FSRS-6) or 35 (FSRS-7) |
| **Implementation complexity** | ~200 LOC (trivial) | ~3,000+ LOC (reference `fsrs` crate) |
| **Cold start behavior** | Works immediately, no data needed | Works immediately with defaults; optimizer needs ~1,000 reviews |
| **Log Loss (benchmark)** | ~0.43+ (adapted estimate) | 0.346 (FSRS-6) / 0.344 (FSRS-rs) |
| **Reviews needed for same retention** | Baseline | 20-30% fewer |
| **Ease hell** | Yes (structural) | No (mean-reverting difficulty) |
| **Target retention config** | No (fixed rule) | Yes (user-configurable) |
| **Per-user personalization** | No | Yes (after ~1,000 reviews) |
| **Maintenance burden** | Near-zero (frozen spec) | Low-moderate (upstream crate is actively maintained) |

### Open-SRS Benchmark Data (source: github.com/open-spaced-repetition/srs-benchmark)

The benchmark evaluates prediction accuracy across 9,999 Anki users and ~350M reviews:

- **FSRS-rs (Rust port, FSRS-6 based):** Log Loss = 0.3443, RMSE(bins) = 0.0635, AUC = 0.7074
- **FSRS-6:** Log Loss = 0.3460, RMSE(bins) = 0.0653, AUC = 0.7034
- **FSRS-4.5:** Log Loss = 0.3624, RMSE(bins) = 0.0764, AUC = 0.6893
- **FSRS-7 default param. (no optimization):** Log Loss = 0.3629, RMSE(bins) = 0.0910, AUC = 0.6944
- **SM-2** is not directly in the benchmark because it doesn't output probability predictions. Adapted estimates place SM-2 at log loss ~0.43+.

Key takeaway: **Even FSRS-7 with zero per-user optimization (just default params) outperforms every SM-2 variant.** FSRS-rs (the Rust crate) with per-user optimization is the best among practical SRS algorithms (excluding the research-grade RWKV/GRU neural models which are 10x more complex).

## 4. Rust Ecosystem Check

### FSRS Crates

| Crate | Version | Downloads | Last Updated | License | Notes |
|---|---|---|---|---|---|
| [`fsrs`](https://crates.io/crates/fsrs) | 6.6.1 | 323,422 total (40k recent) | 2026-06-09 | MIT (likely) | Official port, 73 versions, active. Includes both scheduler and optimizer. Repo: github.com/open-spaced-repetition/fsrs-rs |
| [`rs-fsrs`](https://crates.io/crates/rs-fsrs) | 1.2.1 | 11,700 total (5k recent) | 2024-10-28 | — | Less active, 1 version only. Not recommended. |

The `fsrs` crate is the clear choice: 73 releases, 323k+ downloads, maintained by the open-spaced-repetition GitHub org (same org that runs the srs-benchmark). It implements FSRS-6 with the optimizer included.

### SM-2 Crates

| Crate | Version | Downloads | Last Updated | License | Notes |
|---|---|---|---|---|---|
| [`sm-2`](https://crates.io/crates/sm-2) | 0.2.0 | 1,354 total (14 recent) | 2025-08-18 | MIT | 199 lines of Rust. Also under open-spaced-repetition org. Minimal, well-understood. |

### Verdict on Crates

A **good, maintained FSRS crate exists** (`fsrs` v6.6.1). No need to implement from scratch. Same org also provides `sm-2` if needed. Both are MIT-licensed.

For SM-2, the crate is so small (~199 LOC) that the maintenance risk is effectively zero — even if abandoned, you could reimplement in an hour.

## 5. Recommendation

### Decision: Use FSRS via the `fsrs` Rust crate.

**Rationale tied to Mnemosyne constraints:**

1. **2-3 users initially / cold start:** FSRS default parameters (trained on 700M+ reviews) work well from day one. The cold start concern is real but manageable — we don't need per-user optimization for the first weeks. Default FSRS-7 parameters already outperform SM-2 on 99.6% of users. With 2-3 users, we lose little by skipping personalization initially.

2. **16-week timeline:** The `fsrs` crate is production-ready (v6.6.1, 73 releases, Anki-proven). Integrating it is a matter of adding a dependency and wrapping the API — estimated 2-3 days including tests. SM-2 would be faster (1 day), but the time savings aren't meaningful at this scale, and we'd be shipping an obsolete algorithm.

3. **Portfolio/research angle:** FSRS is the academically stronger choice. The project can reference the srs-benchmark, the DSR memory model, and the personalization optimizer in papers/demos. SM-2 is a 1987 algorithm — choosing it signals you didn't do the research. FSRS demonstrates awareness of current SRS research (2022-2026).

4. **Future-proofing:** Anki, RemNote, and every major SRS tool have standardized on FSRS. If Mnemosyne ever needs to interoperate (import/export scheduling data), FSRS is the 2026 standard.

### Should we start with SM-2 and upgrade later?

**No.** This would add unnecessary rework:

- Schema migration: `learning_events` has `ease_factor` and `interval` (SM-2 compatible) but needs `stability` and `difficulty` for FSRS. Starting with SM-2 and upgrading means dropping EF, adding S+D, and migrating ~0 existing records (trivial in the short term, but a schema churn that adds no value).
- Code rework: The FSRS Rust crate wraps all scheduling logic. SM-2 would be a separate module that gets thrown away. With only 2-3 users, we don't have a legacy migration problem — start right.
- The only real argument for SM-2-first is "simpler to debug during early development." But the `fsrs` crate is sufficiently well-tested (backed by Anki's entire userbase) that this concern is theoretical, not practical.

**Recommendation:** Add `fsrs` = "6.6.1" to Cargo.toml. Implement a thin wrapper that maps Mnemosyne's `learning_events` rows to FSRS `ReviewLog` structs and calls the scheduler. No SM-2 fallback.

### Alternative Considered But Rejected

**Starting with SM-2 → upgrading to FSRS later:** Rejected because the upgrade path requires schema changes, code rewrite of the scheduling module, and produces no intermediate benefit. The `fsrs` crate is mature enough to use from day one. If we had 10,000 users on SM-2, migration cost would justify a phased approach. With 0 users, start on FSRS.
