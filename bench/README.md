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

## Score interpretation

This is a self-benchmark on a synthetic corpus. It is honest about gungnir
and mechanical to reproduce against anything else, but it is not LongMemEval
and it is not a claim about real-world recall. Treat cross-store numbers as
"does the store express these abilities at all," not leaderboard ranking.

## Files

- `corpus.yaml`: the world under test. Two agents, three sessions, a
  supersession chain, a verified-then-rolled-back fact, and one unrelated
  verified fact.
- `checks.yaml`: the 19 assertions, grouped by ability.
- `../tests/bench.rs`: the reference runner; parses both files and scores
  gungnir against them on every CI run, so the artifact cannot drift from
  what the test asserts.
