# gungnir-bench v1

A deterministic benchmark for agent memory stores. Fixed corpus, fixed
checks, exact-match grading. No LLM judge, no ambiguity, nothing to game.

The corpus and checks are data, not code. Any memory system can run them:
load `corpus.yaml` into your store, execute `checks.yaml`, count passes.
The reference runner is this repo's `tests/bench.rs`
(`cargo test --test bench`), which runs the same files against gungnir in
CI.

## Why this exists

Published memory benchmarks measure against LLM judges or vendor-run
harnesses. Zep's LoCoMo score has been published at values from 58 to 95
depending on whose harness ran it; Mem0's published numbers are
unreproducible at home; LoCoMo's own answer key is ~6% wrong. A benchmark
that cannot be gamed has to be mechanical: the corpus is checked in, the
checks are checked in, and every assertion is a string match or a count
any reviewer can recompute by hand.

## What it measures

Five abilities, borrowed from LongMemEval's taxonomy and rebuilt as
deterministic checks:

- **Information extraction.** A finding promoted to the shared layer is
  retrievable, and its provenance link resolves.
- **Multi-session reasoning.** One briefing surfaces both a failed attempt
  and the later success, and one agent's private journal never leaks into
  another's.
- **Knowledge updates.** Superseded facts stop posing as current; coverage
  counts say so.
- **Temporal reasoning.** As-of queries reconstruct belief at an instant,
  including across verify and rollback transitions.
- **Abstention.** The store reports "nothing verified covers this" instead
  of staying silent when coverage is empty.

19 checks. A score is `passes / 19` plus the per-ability breakdown.

The reference runner also executes a Rust-only `paraphrase recall` suite
(18 checks over retrieval machinery: BM25-lite vs hybrid fusion) in the
same run, so its printed total is 19 + 18 = 37. The public score is the
YAML side — `checks.yaml` is the whole contract.

## The contract

A runner adapts its store to five operations, then executes checks in order
against a fresh store built from `corpus.yaml`.

**Ingest.** For each `sessions[]` entry: record the transcript, then archive
it under `journal_summary` into the agent's private layer. Each `promote[]`
item becomes a shared-layer entry carrying provenance evidence that points
at the session's archive entry; `verify_as` names the verifier to record.
For each `facts[]` entry: create a shared-layer entry; `revises` links to an
earlier label, `at_hours`/`verify_at`/`mark_rolled_back_at` pin times as
hours after `epoch_base`, `mark_rolled_back_as` records the actor of a
rollback mark.

**Run shapes** (the `run` key of each check):

- `search`: query one layer; assert on hit count, first-hit summary
  content, and topic-scoped coverage counts (`verified`, `unverified`,
  `hidden_superseded`, `hidden_rolled_back`, `total_visible`).
  `current: true` resolves supersession chains to their heads;
  `as_of: N` evaluates the store as it stood N hours after `epoch_base`.
- `brief`: compile the agent's pre-task briefing; assert on its markdown
  text and its leading shared-layer hit.
- `entry`: fetch a labeled entry; assert that its evidence link resolves
  inside the named layer.

If a store lacks a concept (private per-agent layers, coverage counts,
as-of evaluation), the checks that need it score as failures. That is the
point: the abilities are what is being measured, not the check strings.
The schema is strict — unknown fields in either YAML file are rejected, so
a misspelled assertion can never score a silent pass.

## Score interpretation

This is a self-benchmark on a synthetic corpus. It is honest about gungnir
and mechanical to reproduce against anything else, but it is not LongMemEval
and it is not a claim about real-world recall. Treat cross-store numbers as
"does the store express these abilities at all," not leaderboard ranking.

## Entity and session expansion fixture

`cargo run --example gung23_measurement` is a small before/after fixture for
recall expansion, not part of this benchmark and not a full eval. Its eight
cases link a verified neighbor to a lexical seed by an entity string or a
session id, with zero stemmed token overlap between the neighbor and the
query. The baseline is `0/N` because keyword recall cannot recover those
neighbors without expansion. The post-change expectation is `N/N` on the
`current` arm. The example also prints a `require_verified` arm, which stays
lexical by design, so it measures the explicit `0/N` before baseline with a
different semantic role. This is not a benchmark baseline for the same query
semantics.

## Answer-binding fixture

`cargo run --example aev27_measurement` is a targeted before/after fixture
for abstention that tests whether returned evidence binds the question, not
part of this benchmark and not a full eval. Each of its seven cases stores
one verified entry that shares lexical terms with the question, so retrieval
and provenance both succeed and only the binding check separates the legs.
Three cases are unanswerable by design: a question about a manager against
senior-engineer evidence, a question about three cats against a four-cat
note, and a negated question against unnegated evidence. Four are
answerable controls: the matching role, number, and negation cases plus an
appliance question whose only shared words are open-class, so the gate must
stay inert.

The baseline is `0/3` unanswerable empty because keyword recall cannot
refuse a lexical near-miss. The post-change expectation is `3/3` on the
`require_evidence` arm with the control leg unchanged at `0/4` on both
sides. The printed numbers are empty-result counts, not judged-answer
accuracy and not an LME/BEAM score. `src/recall.rs` pins the same behavior
as unit tests, and `tests/audit.rs` pins the CLI contract.

## In-text date fixture and LME adapter boundary

`cargo run --example aev41_measurement` checks four targeted date retrieval
cases and two controls. It covers absolute and relative forms in both
questions and entries without topic-word overlap. Its parent-revision
baseline is `date_recovered=0/4`, `wrong_date_empty=1/1`, and
`lexical_fallback=1/1`. The required result after date selection is
`date_recovered=4/4` with both controls unchanged at `1/1`. The fixture uses
fixed UTC timestamps and explicit question times. It is not an LME or BEAM
run, and it does not measure aggregate benchmark improvement.

An LME adapter maps each question's `question_date` to `Query::question_time`.
It must populate each canonical `Entry.timestamp` from the corresponding
`haystack_dates` value. Entry-relative phrases use that timestamp as their
anchor. `question_time` never replaces `as_of`, which continues to reconstruct
learned-time history. Absolute ISO dates need no question clock. Omitting
`question_time` leaves relative question phrases unresolved.

## Files

- `corpus.yaml`: the world under test. Two agents, three sessions, a
  supersession chain, a verified-then-rolled-back fact, and one unrelated
  verified fact.
- `checks.yaml`: the 19 assertions, grouped by ability.
- `../tests/bench.rs`: the reference runner; parses both files and scores
  gungnir against them on every CI run, so the artifact cannot drift from
  what the test asserts.
