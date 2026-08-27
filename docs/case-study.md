# Mnemosyne: A Case Study in Building an AI-Assisted Learning Platform

*A development case study documenting the design, implementation, and empirical findings from building a cognitive-science-grounded, AI-powered learning platform.*

---

## Abstract

Mnemosyne is a personalized learning platform that operationalizes four cognitive science principles — spaced repetition, active recall, Socratic questioning, and the Feynman Technique — as software features backed by a large language model (DeepSeek V4 Flash/Pro) and the FSRS (Free Spaced Repetition Scheduler) algorithm. This case study documents the system's architecture, the empirical decisions made during development (algorithm selection, prompt engineering iterations, infrastructure debugging), and — most importantly — real, measured behavior of the AI components under deliberately adversarial test conditions. Rather than presenting only a finished system, this document treats the build process itself as a source of evidence: several features initially exhibited AI sycophancy (unwarranted praise, failure to redirect off-topic conversation) that required targeted, verifiable correction. The result is both a working four-endpoint learning system and a small dataset of findings about how LLM-based tutoring behaves under scrutiny.

---

## 1. Introduction

Traditional flashcard and study applications implement, at best, one learning principle — usually spaced repetition — and treat content creation and comprehension checking as entirely the learner's responsibility. This creates two gaps: first, learners must manually author study material, which is a barrier to consistent practice; second, passive review (reading a card, self-grading) captures only the retrieval-practice benefit and none of the benefits associated with generative and dialogic learning strategies (Socratic questioning, self-explanation).

Mnemosyne's premise is that a large language model can serve as a cost-effective, always-available partner for the more labor-intensive parts of these strategies — generating retrieval-practice questions from source material, conducting Socratic dialogue, and evaluating self-explanations — while a deterministic, well-validated algorithm (FSRS) handles the part of the system where algorithmic reliability matters more than generative flexibility: scheduling.

This split is deliberate and is revisited in Section 6: the project treats "which parts of the pipeline should be an LLM" as a design question with a real answer, not a default.

---

## 2. Cognitive Science Foundations

Full literature review and citations are maintained separately in `docs/research.md`; this section summarizes the four principles as implemented.

### 2.1 Spaced Repetition (the Spacing Effect)

Distributing review sessions over time, with increasing intervals, produces more durable memory than massed practice (Ebbinghaus's original forgetting-curve work, later formalized by Cepeda et al.'s meta-analysis). Mnemosyne implements this via FSRS rather than the older SM-2 algorithm (see Section 3 for the selection process), storing a per-card *stability* and *difficulty* estimate that determines the next review date.

### 2.2 Retrieval Practice / Active Recall

Testing oneself on material — rather than re-reading it — produces superior long-term retention (Karpicke & Roediger's testing-effect research; Roediger & Butler on the critical role of retrieval practice). Mnemosyne's AI question generator implements a "recall" mode specifically designed to produce SAFMEDS-style short-answer flashcards (1–5 word answers) rather than essay-style questions, to preserve the rapid-fire retrieval-practice format.

### 2.3 The Socratic Method

Guiding a learner to a conclusion through questioning, rather than direct instruction, is associated with deeper conceptual understanding and better transfer (theoretical grounding in scaffolding and productive-failure literature). Mnemosyne's Socratic Tutor is a multi-turn dialogue endpoint constrained by an explicit system prompt to ask guiding questions and flag — but not directly correct — misconceptions.

### 2.4 The Feynman Technique / Elaboration

Explaining a concept in one's own words surfaces gaps in understanding that passive review does not (elaborative-encoding literature; Chi et al. on self-explanation effects). Mnemosyne's Feynman Evaluation endpoint asks the learner to write a free-text explanation of a study set's material and returns a structured, three-dimension critique (clarity, completeness, correctness).

---

## 3. System Architecture

**Stack:** Rust (Actix-web backend), PostgreSQL, the `fsrs` crate (v6.6.1), and DeepSeek's API (V4 Flash for cost-sensitive tasks, V4 Pro for more demanding reasoning during a rate-limit period). Originally built on Supabase with a planned Leptos frontend; both were dropped in August 2026 when Mnemosyne became a backend module of the Chiron ecosystem, moving to a local Postgres cluster with the UI delegated to the Chiron OS shell.

**Data model (10 tables across 3 migrations):** `users`, `study_sets`, `cards`, `learning_events` (FSRS state per review), `ai_interactions` (a generic AI-call cost/audit log), `socratic_sessions` + `socratic_messages` (multi-turn dialogue state), and `feynman_evaluations` (structured, scored self-explanation history).

Full architecture diagram is maintained in `docs/architecture.md` (Mermaid), with an explicit visual distinction between components that are built-and-tested versus planned-but-not-built, to avoid overclaiming system maturity in project documentation.

### 3.1 The FSRS vs. SM-2 Decision

Rather than assuming a spaced-repetition algorithm, the project ran a dedicated comparison (`docs/spaced-rep-spike.md`) before writing any scheduling code. Key findings, captured in ADR 0001:

- FSRS's DSR (Difficulty-Stability-Retrievability) model outperforms SM-2 on large-scale benchmark data (the `srs-benchmark` dataset, ~9,999 users, ~350M reviews), including under cold-start conditions with default parameters — directly relevant given Mnemosyne's small (2–3 user) initial user base, which cannot support per-user parameter optimization.
- A mature, actively maintained Rust implementation (`fsrs` crate, MIT-licensed, `open-spaced-repetition` org) existed, removing the main practical objection to choosing the more complex algorithm.
- **Decision: FSRS**, implemented as a thin translation-layer wrapper (`mnemosyne-core`) rather than reimplementing scheduling math, to inherit the crate's validated behavior directly.

This decision was validated post-hoc during integration testing: a card's first-review stability value matched `fsrs::DEFAULT_PARAMETERS[2]` exactly, confirming the wrapper invokes the real algorithm rather than a stubbed approximation, and a two-review sequence (Good, then Easy) produced stability growth (2.31 → 3.95), difficulty reduction toward the model's floor (2.12 → 1.0), and interval growth (2 days → 4 days) — all consistent with FSRS theory.

---

## 4. Empirical Findings: AI Behavior Under Adversarial Testing

The most substantive contribution of this project is not the four working endpoints but the process of verifying — rather than assuming — that each AI feature behaved as intended. Three findings are documented here because they generalize beyond this specific project.

### 4.1 Finding: Prompt Style Materially Changes Output Format, But Requires Explicit Anchoring

An early version of the question-generation feature, given only a general instruction to generate "active recall" questions, produced essay-style questions with multi-sentence answers — appropriate for elaborative learning, but incompatible with the SAFMEDS fast-recall format the project's own methodology called for. Adding an explicit word-count constraint ("1–5 words") plus **two concrete few-shot examples** (not just a rule) resolved this completely: a controlled test on identical source material produced 1-word answers ("1789", "1793", "1804") in recall mode versus 2–3 sentence causal-analysis answers in elaboration mode, from the same underlying model and the same source text.

**Implication:** a stated constraint ("keep answers short") was insufficient on its own in earlier informal testing; the combination of an explicit format rule *and* worked examples was what reliably shifted output format. This is consistent with the broader observation that few-shot demonstration constrains LLM output more reliably than instruction alone.

### 4.2 Finding: LLM-as-Tutor Exhibits Measurable Sycophancy, and Prompt-Level Fixes Require Iteration

The Socratic Tutor's first verification run included a deliberately off-topic student response (discussing the French Revolution when asked about a different historical event). The model's reply — "You've raised good points about the French Revolution" — praised an answer that did not engage with the question at all.

A first attempted fix (adding a single explicit rule: "point out off-topic answers, don't praise them") **partially worked**: the model stopped praising the off-topic answer, but continued to *engage* with the off-topic content rather than redirecting the learner back to the original question. Root-cause analysis identified a conflict between the new rule and an existing rule ("build on what the student says") — the model appeared to resolve the conflict in favor of the older, more general instruction.

A second iteration resolved this by (a) adding explicit rule-precedence language ("rule 4 takes priority over rule 5 when off-topic"), (b) requiring the model to *demonstrably* restate the original question in its reply (making the redirect verifiable from the output alone, not just inferable), and (c) narrowing the scope of the conflicting rule ("build on what the student says *about the current question*"). A repeated test of the identical scenario showed the model now explicitly naming the mismatch and restating the original question verbatim.

**Implication:** single-rule prompt patches are not reliable for correcting subtly conflicting instructions in a multi-rule system prompt; diagnosing *which* existing rule the model is over-weighting, and adding explicit precedence, was necessary. This took two verified iterations, not one — the first "fix" would have shipped as broken if not independently re-tested with the same adversarial scenario rather than accepted on the model's own summary.

### 4.3 Finding: LLM-as-Grader Can Discriminate Quality Without Prompting Alone Guaranteeing It

The Feynman Evaluation feature's most important validity question was not "does it work" but "does it actually discriminate between weak and strong student work, or does it inflate all scores toward the middle-high range out of politeness" (a documented failure mode for LLM graders). This was tested directly: a deliberately shallow, vague explanation of the same material scored 5/2/7 (clarity/completeness/correctness) against a deliberately thorough explanation's 10/9/10 on an identical rubric and identical source material.

The *pattern* of the score gap, not just its existence, is informative: completeness showed the largest gap (2 → 9), correctness the smallest (7 → 10). This is the theoretically correct pattern — the weak explanation was imprecise and shallow but not factually wrong ("Balboa discovered the Pacific Ocean" is approximately accurate), so a grader that penalized it primarily on correctness would be measuring the wrong construct. The AI's largest penalty landed on completeness, which is the dimension most directly tied to the Feynman Technique's core diagnostic claim: an explanation that omits the substance of the material reveals incomplete understanding, independent of whether what *was* said was accurate.

**Implication:** unlike the Socratic redirect case, this feature's first prompt version passed its adversarial test without requiring iteration — suggesting that explicit, dimension-by-dimension rubric criteria (rather than a single holistic "score this explanation" instruction) may be more resistant to LLM-grader inflation than single-axis evaluation prompts.

---

## 5. Infrastructure Findings

Two infrastructure issues, while not the project's research focus, consumed meaningful debugging effort and are documented (`docs/gotchas.md`) as they are likely to recur for other developers using this stack combination:

1. **Supabase direct-connection hostnames are IPv6-only** on standard projects; any development environment without IPv6 routing must use Supabase's connection pooler (IPv4-compatible) instead. *(Historical: Mnemosyne no longer uses Supabase.)*
2. **sqlx's default named prepared statements collide with Supabase's pooler in transaction mode**, producing intermittent `prepared statement already exists` errors under connection reuse. The fix (`.persistent(false)` on every query) is a known pattern for PgBouncer-style poolers but is not sqlx's default behavior, making it an easy regression for future code unless explicitly checklisted. *(Historical: with the move to a direct local Postgres connection, the workaround was removed — see `docs/gotchas.md`.)*

---

## 6. Discussion: What Should and Shouldn't Be an LLM

The project's architecture reflects a deliberate split, and the empirical findings above support it retrospectively:

- **Scheduling (when to review) is not an LLM task.** It is handled by a validated, deterministic algorithm (FSRS) with published benchmark accuracy. An LLM asked to "decide when this card should be reviewed next" would introduce exactly the kind of unverifiable, unbenchmarked judgment this project's testing methodology was built to avoid.
- **Content generation and dialogue *are* appropriate LLM tasks**, but only with (a) structured output contracts (JSON schemas, not free text) enabling programmatic validation, (b) database-level constraints as a second line of defense (e.g., `CHECK (score BETWEEN 1 AND 10)` on Feynman scores, independent of application-level validation), and (c) adversarial verification before any feature is considered complete — not just "does it run" but "does it behave correctly under a test case designed to break it."

This third point is the project's central methodological claim: AI-assisted software features require a different verification standard than deterministic code. A unit test can prove a sorting function is correct; no single test can prove a tutoring prompt is non-sycophantic. What this project's process demonstrates is that *adversarial, human-reviewed test cases*, applied consistently and re-run after each prompt-engineering change, can surface and help correct these failure modes — but only if someone is deliberately looking for them, since the failure mode (excessive agreeableness) is, by construction, the one an AI system is least likely to self-report.

---

## 7. Limitations and Future Work

- **Sample size.** All findings above come from a 2–3 user closed alpha with a handful of adversarial test cases per feature, not a controlled study. The claims in Section 4 are about *what was observed*, not statistically validated generalizations.
- **No long-horizon retention data yet.** FSRS's scheduling correctness over weeks/months (as opposed to the unit-tested and integration-tested short-interval behavior) depends on the underlying algorithm's own published validation, not on data this project has independently collected.
- **No frontend.** All verification has been performed via direct API calls; user-facing usability has not been evaluated. A frontend is out of scope for this module — UI belongs to the Chiron OS shell.
- **Single-session testing.** The Socratic redirect fix and Feynman scoring pattern were each verified against one adversarial scenario per finding; broader robustness (different subjects, different misconception types, adversarial phrasing variety) is unverified and is a natural next research step, particularly if this system's data is used for a future publication.

---

## Appendix: Development Methodology Note

This system was built through a structured collaboration between a human developer, an AI "technical planner" role (scoping prompts, reviewing outputs, catching scope creep before it reached the database or committed code), and an AI coding agent (OpenCode, using GLM-5.2 for implementation work and DeepSeek V4 Flash/Pro for lighter documentation and research tasks). Every schema change was written but not executed by the coding agent — live database changes were applied only after human review. Every claim of "this works" was required to include pasted, real command output (build logs, HTTP responses, actual generated AI content) rather than a natural-language summary, following the principle that compile-time verification and runtime verification are not interchangeable evidence. This methodology is itself documented as a reusable framework and is available on request.
