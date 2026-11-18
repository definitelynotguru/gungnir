# Design

Gungnir is local-first, markdown-native memory for AI agents. This document
explains why it is built the way it is. The full behavioral contract lives in
[SPEC.md](../SPEC.md); integration guides live in [INTEGRATIONS.md](INTEGRATIONS.md);
the trust model lives in [PROVENANCE.md](PROVENANCE.md).

## Goals

1. Any agent framework adopts it with one import or one MCP wire.
2. Provenance is a first-class field, never an afterthought.
3. Rollback is non-destructive and operator-safe.
4. Everything on disk is human-readable markdown, git-friendly, no binary index.
5. Zero network calls in the default build.

## Layer partitioning

Memory splits into three directory partitions under one root:

```
<root>/codex/<id>.md              shared source of truth
<root>/journal/<agent>/<id>.md    private per-agent history
<root>/scratch/<session>/<id>.md  ephemeral per-task working memory
<root>/.cache/embeddings/...      derived vectors, safe to delete
```

Directories were chosen over a layer field inside one pool of files for three
reasons.

- **Privacy becomes configuration.** Committing only the codex is one
  `.gitignore` line. No query-time permission checks to get wrong.
- **Lifecycle is mechanical.** Clearing scratch after a task is `rm -r` on one
  directory. No garbage collection pass that might touch shared facts.
- **One storage engine.** All three layers use the same `Store`: atomic writes,
  date sharding, validation. Layers differ by path, not by code path.

## Identity

Entry ids are ULIDs: 48-bit millisecond timestamp followed by 80 random bits,
encoded as 26 characters of Crockford base32. Consequences:

- Lexicographic order equals chronological order, so listings sort without
  reading file contents.
- Filenames are filesystem-safe on every platform.
- Collision probability is negligible; no retry loop exists in the codebase.
- The id embeds its creation time, which decides its date shard
  (`YYYY/MM/DD`) deterministically regardless of write order.

There are deliberately two clocks. The ULID's embedded time places the file
(the shard), and `entry.timestamp` — initialized to the same instant —
drives `as_of` candidate filtering. Both are **learned time**: `as_of`
reconstructs what the store believed at an instant, evaluated from
`timestamp` plus the append-only `verification_log`, not what was true in
the world at that instant. An event-time field would be a second axis;
nothing models it today.

## Entries

One entry is one markdown file: YAML frontmatter plus a free-form body. The
complete field reference is in [SPEC.md](../SPEC.md). Two structural choices
matter more than the rest.

**Verification is a state machine, not a flag.** An entry is born
`unverified`. Reaching `verified` requires an explicit logged transition.
`contradicted` must name the entry that contradicts it. Illegal states are
unrepresentable at the type level, so no write-time rule needs to enforce
them.

**Status carries its payload.** An open entry cannot exist without an owner
because the `open` variant contains the assignee field. Validation code never
checks "is assigned_to present when status is open"; it cannot be otherwise.

## Validation

Rules are pure functions over an entry plus an existence resolver. The store
supplies "does this id exist here" during single-layer writes; the facade
supplies "does this id exist anywhere" during cross-layer writes. The same
rules govern both, and adding a rule means touching one file.

## Recall

Two retrieval modes share one contract: the same temporal candidate pool
(`as_of` filters by entry timestamp and evaluates verification from the log,
`current_only` resolves revises chains to heads), the same coverage
accounting, and the same ordering rule.

**Keyword mode (default, zero dependencies)** is BM25-lite: query and entry
text tokenize through a small English stemmer ("migrating" meets
"migration"), then score as `idf × tf` over summary (weight 2) and body with
length normalization. Entries are already loaded for the candidate pool, so
document frequency is free — no index, no persistence.

**Hybrid mode (opt-in)** fuses the keyword ranking with embedding cosine
ranks through reciprocal rank fusion (k = 60). A candidate counts as
topic-matched when it has signal in either channel: a keyword hit, or
cosine ≥ 0.45, a floor that keeps near-random similarity from manufacturing
coverage. Hybrid runs when the caller passes `--semantic` (CLI) or
`semantic: true` (MCP) and an embedder is available; the `embed` cargo
feature ships one — bge-small-en-v1.5 quantized via fastembed/ONNX, ~100MB
of weights downloaded on first use into `.cache/fastembed`. Without the
feature, semantic requests error rather than silently degrading to keyword.

**Standing-constraint lane (both modes).** A deterministic cue tagger
(`always`, `never`, `from now on`, `I prefer`, `don't`, and close cousins)
marks entries as standing rules. Only verified entries are eligible, so
unverified and contradicted content cannot steer a task briefing. The lane is
a derived view rebuilt from the canonical files on each query — no second
store, no model on the write path, and a cue match never verifies an entry.
A query-shape router unions a capped slice (at most 4) into **task-shaped**
queries only: queries that ask the agent to produce or act, with no fact cue.
Single-word task cues stem their word ("building" meets "build"); phrase cues
stay exact. Fact questions (temporal and
knowledge-update included) and all `as_of`/`current_only` queries are
fact-shaped and get no lane, so generic rules cannot crowd their top-k. The
lane appends after the ordered lexical hits into spare slots below `limit`;
it never evicts or reorders a lexical hit, and lane entries carry score 0.0.
Cues match whole words in either field, never across the summary/body
boundary. Lane hits are not topic-matched coverage, so a briefing that lists
them reports them instead of claiming no knowledge matched.
`require_verified` stays purely lexical, so strict provenance queries never
union standing guidance.

**Entity and session expansion (both modes).** BM25 returned only the turns
that shared question words (mean source coverage 0.09 and 0.06 on
summarization and event-ordering probes). Recall now derives one adjacency
map per query from the temporal candidate pool: keys are normalized
`entity_mentions()` strings (frontmatter `entities:` plus name-shaped
`[[wikilinks]]`) and `session_id`, under separate prefixes so the namespaces
cannot collide. Every ordered hit with positive lexical or vector relevance
seeds one hop to verified neighbors sharing a key, so expansion follows
topical relevance rather than standing cues. Seeds and existing hit ids are
excluded; unverified, contradicted, and rolled-back neighbors never enter.
`require_verified` stays lexical. At most `EXPANSION_LANE_CAP = 6` neighbors
append after the ordered hits and the standing lane, under `limit`, with
score 0.0 and no coverage change. The slice is deterministic (newest
`timestamp`, then descending `EntryId`). `as_of`, `current_only`, rollback,
and latest-slot resolution stay in force because the lane reads the same
visible pool. Keyword and hybrid recall share `finish`, and `embed` stays
opt-in with no embedder synonym seed. This is a read-path derived walk over
canonical entries; the deliberate refusal of a persisted graph and OpenIE
still stands.

**Latest-slot resolution (both modes).** An explicit `revises` chain is one
slot. The head is the member no live `revises` link points at, computed once
in `temporal_pool` as the link-target set. `current_only` hides that set; a
plain present-time query instead demotes the verified tails in it below
current knowledge even when their wording scores higher on BM25. Both
generations stay visible on a plain read; the two views share one head
definition, so an inverted link cannot make them name opposite heads. Only
verified tails are demoted, so an unverified proposal can never displace
verified knowledge. A `kind = rollback` record and rolled-back members
contribute no links, matching every other supersession consumer: a
rolled-back head falls back to the last live member, and a cycle marks both
members as link targets without aborting the read. `as_of` and
`current_only` skip the demotion, so historical ordering and the strict
current view are unchanged. The rule is purely a read-path sort key: files
stay the store of record, no link is written from an unverified proposal, no
verification state changes, and no embedder or LLM judge is involved. It
lives in the shared ranking step, so keyword and hybrid recall cannot
disagree about which member is current.

**Scoring formula, both modes.** Final order is a verification bucket
(verified > unverified > contradicted) sorted lexicographically first, then
a slot-staleness tie-break inside that bucket, then the score
`relevance × trust × recency`:

- `relevance`: BM25-lite for keyword, RRF score ×1000 for hybrid.
- `trust`: 1.0 verified, 0.75 unverified, 0.35 contradicted. Because the
  bucket sorts first, a contradicted entry can never outrank a verified one
  on the same topic regardless of relevance or slot membership.
- `recency`: exponential decay in entry age at the query instant,
  half-life 90 days, floored at 0.25 so old-but-true facts keep a floor.

The embedding cache is content-addressed: `sha256(model + "\n" +
normalized_text)`. Editing a file invalidates its vector naturally. Switching
models partitions the cache by directory. Nothing keys on mtime, because
mtimes lie under git operations, sync tools, and restores.

Hybrid recall is O(N) cosine over the candidate pool per query — fine to
roughly 10k entries. Past that the "no binary index" non-goal below needs
revisiting: an ANN index or a vector store becomes worth its corruption
modes. Until then a linear scan keeps the files canonical and the code one
page long.

## Rollback

Non-destructive by contract. Rolling back a target walks the `revises` chain
backward to the first verified ancestor, marks every intermediate rolled back
by appending a log record, and writes a new rollback entry pointing at that
ancestor. No file is ever deleted or rewritten destructively. A missing
verified ancestor aborts the plan before any mutation. Cycle detection guards
against corrupted chains.

Plan and apply are separate functions, so tooling can preview a rollback
before executing it.

## Conflict candidates

A write that disagrees with a live verified fact used to coexist with it
silently. Now every claim write (`add`, `supersede`, `promote`, and the
promotions inside `end_session`) lands first, then the store scans for
conflict candidates: *advisory* records under `<root>/.conflicts/`, one
markdown file per candidate. Detection never blocks a write — a conflict is
a queue item, not an error.

**What counts as a candidate.** Only a *live verified* fact can be
conflicted with — superseded tails, contradicted, and rolled-back entries
are out, which falls straight out of running detection under the same
`current` temporal view recall uses. The write is scanned against its home
layer plus the Codex (a journal fact disagreeing with a verified Codex
claim is exactly what the loop exists to surface; one agent's journal never
scans another's). Scratch observations never raise candidates — they are
working memory, not claims — and neither do `Session` archives or
`Rollback` records.

**The signals are deterministic and documented on the record.** Over
stemmed summary tokens: `shared_frame` (≥2 shared topic tokens and private
content on both sides — "sessions use mysql" vs "sessions use postgres"),
`negation_asymmetry` (one side negates), and `numeric_mismatch` (different
digit-bearing tokens). Each record carries the signal that raised it, so a
reviewer sees *why* the candidate exists. The trade is deliberate: flag the
same-frame divergence and let people dismiss the noise, rather than miss a
real contradiction. Near-duplicates never flag — an identical token set is
corroboration (the semantic channel judges it over the full claim text,
summary and body, so same-summary writes with disagreeing bodies still
flag). With the `embed` build, `--semantic` on `add`/`supersede`/
`promote` adds a fourth signal, `semantic_overlap` (cosine ≥ 0.70), which
catches paraphrased contradictions at a higher false-positive cost.

**Resolution is the trust model's existing vocabulary, not a new state.**
`gungnir conflicts resolve <id> --supersede|--contradict|--dismiss`:
supersede links the new entry as the revision of the conflicting one (the
same end state write-time `supersede` produces); contradict marks the
verified entry `contradicted_by` the new claim through the same logged
transition; dismiss closes the record without touching entries. Resolution
is recorded on the candidate (`resolved_by`, `resolved_at`), and a resolved
record can never be re-resolved.

**Why files and not virtual state.** A candidate has a lifecycle:
`dismissed` and `resolved-*` are terminal states no scan can reconstruct.
A `.conflicts/` markdown queue persists them in the substrate the product
already sells — git-diffable, human-editable — rather than hiding them in a
derived index.

## Audit and abstention

The trust lifecycle only pays if a human can read it. `src/audit.rs` is the
read surface: every audit command is a pure function over the same
`Store::entries()` scan recall already performs — `log <id>` walks the
verification log and the `revises` lineage, `log --since` scans creations
and verification transitions chronologically across all partitions, `diff
<t1> <t2>` replays the as-of machinery at two instants and reports the
delta, `blame` resolves an id or a topic to its vouching and supersession
lineage, and `review` lists unverified belief-kind entries for the daily
triage ritual. Snapshot semantics deliberately mirror `current_only`: a
rolled-back head reveals its tail, and a rollback record's `revises` pointer
restores rather than replaces, so `diff` and recall can never disagree about
what "superseded" means.

Entities stay entity-lite on purpose: frontmatter `entities:` names plus
`[[name]]` body mentions merge into the index, and `[[id]]` wikilinks are
validated at write time exactly like evidence refs. Recall derives a one-hop
read-path walk from these names and `session_id` on each query (see
"Entity and session expansion"), but the store keeps no persisted graph and
does not run OpenIE. That refusal still stands.

Abstention is a first-class output, not a prose aside. `require_verified`
filters hits to verified entries while coverage keeps counting the full
matched pool, so "nothing verified" and "nothing exists" never collapse. The
CLI turns it into an exit code (2) plus an explicit verdict line; `recall
--json` and the MCP `coverage:` line carry `abstained` for callers that
branch on it programmatically.

## Concurrency and durability

Writes go to a temporary sibling then `rename` into place, atomic on POSIX and
Windows. Multi-entry operations hold an exclusive `flock` on a root lockfile,
serializing writers across processes. A crash mid-write leaves the previous
version intact plus an orphaned `.tmp-*` file; the next `Store::open` sweeps
temporaries older than sixty seconds, so a live writer's temp is never touched.

## Surfaces

- **Library.** The `Gungnir` facade owns all high-level operations. CLI and
  MCP stay thin over it.
- **CLI.** Human-first output, composable exit codes, session lifecycle
  commands.
- **MCP.** Hand-rolled newline-delimited JSON-RPC over stdio. The protocol
  surface needed (initialize, tools/list, tools/call, ping) is small enough
  that an SDK plus async runtime costs more than it saves. Tool errors return
  as results with `isError: true` per the spec; malformed requests get
  JSON-RPC error objects.

## Non-goals

- No daemon or background process. The store is a directory; tools open it,
  act, close it.
- No server-side anything. Local-first means your disk, your git remote.
- No binary indexes. At realistic fabric sizes, keyword scan over small
  markdown files is fast; an index would add corruption modes for speed nobody
  needs yet.
- No automatic promotion from journal to codex. Promotion is a judgment call
  and stays deliberate.
