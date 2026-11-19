# gungnir — behavioral spec

Local-first, markdown-native memory for AI agents. Original Rust implementation;
derived from public documentation of prior art, not from its source code.

## Purpose

Give agents persistent, sourced, version-controlled memory across sessions:
stop re-using stale facts, repeating failed attempts, contradicting decisions.

## Layers

1. **Scratch** — per-task working memory. Observations, attempts, hypotheses.
   Ephemeral; archived into the Journal when the task ends.
2. **Journal** — per-agent private history of what was tried, what worked,
   what failed. Only visible to the agent that produced it.
3. **Codex** — shared, topic-organized source of truth. Facts link back to the
   entry that introduced them. Superseded facts are marked, never overwritten.

Before each task the library compiles a **Briefing**: current Codex facts for
the task topic + relevant prior attempts from the caller's Journal.

## On-disk layout

```
<root>/
  YYYY/MM/DD/<id>.md        # entries, YAML frontmatter + markdown body
  .cache/
    embeddings/<model>/<id>.json   # vector + key metadata
```

Root resolution order: explicit argument → `GUNGNIR_ROOT` env → `~/.memsys`.

## Identity

- Entry ids are **ULIDs** (`01JZZZZZZZZZZZZZZZZZZZZZZZ`, 26 chars, Crockford base32).
  Time-sortable; lexicographic order ≈ chronological order.
- Filename == id. No filesystem-hostile characters.

## Entry shape

YAML frontmatter + markdown body. Fields:

| field             | type                | notes                                   |
|-------------------|---------------------|-----------------------------------------|
| `id`              | ulid                | immutable                               |
| `agent`           | string              | required                                |
| `kind`            | enum                | decision \| observation \| attempt \| review \| rollback |
| `summary`         | string              | required, ≤ 200 chars                   |
| `timestamp`       | RFC3339 UTC         | required                                |
| `status`          | open \| closed      | `open` requires `assigned_to`           |
| `assigned_to`     | string?             |                                         |
| `project_id`      | string?             |                                         |
| `session_id`      | string?             |                                         |
| `revises`         | id?                 | must reference an existing entry        |
| `review_of`       | id?                 | required when `kind = review`; must exist |
| `verification`    | unverified \| verified \| contradicted \| rolled_back | |
| `contradicted_by` | id?                 | required when verification = contradicted |
| `verification_log`| list of records     | `{verifier, timestamp, status, note}`; `status` is one of the four states, other values fail to parse |
| `evidence`        | list                | `{kind: file, path, excerpt ≤ 500, sha256}` or `{kind: ref, id}` (refs must exist) |
| `source_tool`     | string?             | `extract` marks machine-originated writes |
| `extracted_by`    | string?             | the extraction backend that proposed the entry |
| `outcome`         | bool?               | whether an `attempt` entry succeeded    |
| `entities`        | list of strings     | entity names, 1..64 chars each          |

Body: free-form markdown after the closing `---`. `[[target]]` wikilinks in
the body are parsed: id-shaped targets are links (validated like evidence
refs), other targets count as entity mentions, merged with `entities` when
the entity index is scanned.

## Write-time validation

- id unique within store
- every referenced id (`revises`, `review_of`, `evidence[].ref`,
  `contradicted_by`, body `[[id]]` wikilinks) resolves to an existing entry
- `open` ⇒ `assigned_to` present
- `kind = review` ⇒ `review_of` present
- entries are born `unverified`; `verified` reachable only via `verify()`
- length caps enforced (summary 200, excerpt 500, entity name 64)

## Recall

Default mode: BM25-lite keyword scoring — `idf × tf` across `summary`
(weight 2) + body, length-normalized, over a stemmed, stopword-stripped
vocabulary so inflections collapse ("migrating" matches "migration").
Filtering happens before scoring. Sort key: `(verification_bucket,
slot_stale, final_score)` where bucket orders verified > unverified >
contradicted, `slot_stale` demotes a verified slot tail below every
non-stale verified entry in that bucket (not just its own slot head), and
`final_score = relevance × trust × recency` — trust weights the bucket
(1.0 / 0.75 / 0.35) and recency decays at a 90-day half-life floored at
0.25. Rolled-back entries are always hidden.

**Latest-slot resolution (plain present-time queries).** One explicit
`revises` chain is one slot, derived from the canonical files on every read.
The head is the member that no live `revises` link points at — the same
`temporal_pool` link-target set that `current_only` hides. A plain query
demotes every verified tail in that set below current knowledge, so a stale
wording cannot outrank a current fact on BM25 score. Both generations stay
visible on a plain read (nothing is deleted or hidden), and `--current`
still hides all superseded tails and counts them as hidden. The two views
share one head definition, so an inverted link cannot make them disagree.
Only a live verified tail is demoted: a slot with no verified tail keeps the
ordinary trust-weighted order, and an unverified proposal never displaces
verified knowledge. A `kind = rollback` record and members evaluated
`RolledBack` do not contribute links; a rolled-back head falls back to the
last live member, and a `revises` cycle degrades safely (both members are
link targets). `as_of` reads skip the rule entirely. This is a read-path
rule only: files stay canonical, no `revises` link is written from an
unverified proposal, and no verification state changes. The shared ranking
step applies it to keyword and hybrid recall alike.

Temporal modes:

- `as_of <instant>`: candidate pool limited to entries whose `timestamp`
  is at or before the instant, and each entry's verification state
  evaluated from its append-only verification log as it stood then.
  `timestamp` is **learned time** — when the fact was written, matching the
  ULID's embedded time — not when the event happened in the world. Asking
  "as of T" answers "what did the store believe at T", not "what was true
  at T".
- `current_only`: every revises chain resolves to its head; superseded tails
  and contradicted facts are excluded from hits and counted in coverage.
  Latest-slot resolution is skipped here because tails are already removed.
- `as_of` and any point-in-time view skip latest-slot resolution entirely:
  historical ordering is reconstructed from the pool at the cutoff, never
  from present-day slot state.

**In-text date retrieval.** `Query::question_time` is an optional UTC instant.
The CLI exposes it as `recall --question-time` and MCP recall as
`question_time`, both parsed from RFC3339. It resolves relative dates in the
question. `None` leaves those phrases unresolved and never reads the wall clock. This clock is independent
of `as_of`, which still controls learned-time admission and verification
history.

Recall indexes dates from each temporal-pool entry's summary and body in
memory on every query. Absolute ISO `YYYY-MM-DD` dates need no anchor. Entry
phrases such as `10 days ago`, `a week ago`, and `four weeks ago` resolve
against that entry's `timestamp`. The grammar accepts positive decimal counts,
the words one through twelve, `a` or `an`, and singular or plural `day` and
`week` with matching count agreement. Invalid calendar dates, unsupported
units, disagreement, adjacent alphanumeric ISO text, and arithmetic overflow
are non-matches. Summary and body phrases cannot span their boundary.

Parsed query dates union their entry-id hits. For a relative day or week
query with explicit `question_time`, recall also matches the UTC calendar date
of each canonical `Entry.timestamp`. The target date is the `question_time`
date minus the parsed interval. The inclusive window extends one calendar day
before and after the target. These timestamp matches need no date text in the
entry. Absolute-date-only queries do not match timestamps this way.

When at least one hit exists, only those entries form the direct matched pool.
Each receives `1.0` plus its existing keyword or hybrid relevance before the
ordinary trust and recency calculation. Date-only hits therefore have positive
score and coverage. Exact target-date hits rank before one-day tolerance hits,
after verification bucket and latest-slot staleness and before final score.
Existing relevance breaks ties within each group. No parsed dates or no index
hits preserve ordinary keyword and hybrid behavior. The maps are derived, are
never persisted, and write nothing. They add no RRF channel and do not bypass
rollback, current views, latest-slot resolution, strict provenance, answer
binding, standing constraints, or verified-seed expansion. Unverified and
contradicted date hits cannot seed expansion. Timestamp selection reads neither
verification-log ingest time nor the wall clock, and it does not change `as_of`.

When date selection finds an index hit, valid absolute ISO spans in the
question are masked before generic numeric constraint extraction. Equivalent
relative evidence can therefore bind an absolute question date. A date miss
keeps the original numeric constraints on the lexical fallback path. Other
role, number, and negation constraints are unchanged.

An LME adapter maps `question_date` to `question_time`. It must populate
`Entry.timestamp` from `haystack_dates` so entry-relative phrases have the
correct anchor. Neither input changes `as_of`.

Every search reports topic-scoped coverage counts (verified / unverified /
contradicted, plus hidden superseded and rolled-back totals, plus the total
entries in scope). Callers use coverage to abstain: when zero verified
facts match, briefings say so plainly — and distinguish "entries matched
but none are verified" from "nothing matched".

Reads are resilient: a ULID-named file that fails to parse is skipped and
reported (stderr on the CLI, a note in MCP responses, `skipped` in library
results) rather than aborting the whole layer scan. Write-time validation
of the entry being written still fails hard.

Promotion accepts journal sources only — a codex source would cite its own
layer and a scratch source would dangle once the session ends.

## Sessions

`session start` persists the session identity (agent, task, start time) to
`scratch/<id>/.session.yaml`, so the archived journal entry carries the
real task text even when the ending process only knows the session id.
`end_session` is idempotent: re-ending an already-archived session reports
`already_ended` instead of writing a second journal entry.

Briefings and `stats --topic` additionally report cross-journal coverage:
aggregate counts over journal partitions other than the caller's (agents
who attempted the topic, agents with a recorded failure, matched entries).
Only counts cross the privacy boundary: peer summaries, bodies, and ids
never surface; topic existence per agent is not hidden.

`require_verified` restricts hits to verified entries while coverage still
counts the whole matched pool, so abstention stays honest about what exists.
CLI `--require-verified` on `recall`/`brief` prints an explicit verdict and
exits 2 (a machine-checkable abstention) when no verified knowledge covers
the topic; `recall --json` emits `{hits, coverage, abstained}`, and MCP
`recall` always ends with a `coverage:` JSON line carrying the same fields.

`require_evidence` (Query `require_evidence()`, CLI `--require-evidence`,
MCP `require_evidence`) first admits a hit only when that entry's summary and
body carry every closed-class constraint extracted from the question. If no
single hit qualifies, disjoint numeric clauses can bind across topical hits
under the rules below.

- Curated role and title modifiers: senior, junior, manager, lead,
  principal, staff, director, chief, intern, associate, assistant, vp,
  president, distinguished. A modifier counts only in a title pattern such
  as "lead engineer", "senior software engineer", "engineering manager", or
  "manager of engineering". Evidence uses the exact modifier or a common
  plural noun. Thus "managers" binds
  "manager", while the verb "leads" does not bind "lead".
- Digit-bearing tokens: leading zeros, numeric ordinals, comma groups, and
  compact or spaced times bind their equivalents. "03", "3rd", and "3"
  bind, "3,000" binds "3000", and "3pm" binds "3 pm". Other digit-bearing
  tokens stay exact. Relative times such as "3 days ago" and list framing
  such as "top 3" are not answer constraints. After date selection, a valid
  absolute ISO date is not an additional answer constraint.
- Negation in yes/no questions. A negated yes/no question requires negated
  evidence. Negated wh-questions and negation on the opening verb do not.
  `no`, `not`, `never`, `none`,
  `nobody`, `nothing`, `neither`, `nor`, `without`, and unapostrophized
  contractions count alone. Apostrophe contractions count as two-token
  phrases, so bare "don" and "won" are words, not negations. The
  apostrophized "can't" is not recognized because its first token is "can".
  The unapostrophized "cant" is recognized. "No longer" is an update cue and
  does not require negated evidence.

Single-entry binding does not require open-class words, so paraphrase remains
eligible. Extra modifiers and numbers are allowed. A question with no
closed-class constraints does not filter any hits, but an empty result still
abstains.

Joint binding applies only to numeric-only questions when per-entry binding
returns no hits. The parser splits at `and` and `or`. It requires at least two
disjoint clauses with at least two numbers each, covering every numeric
constraint. Each clause must have an entry whose summary and body carry its
complete number group and at least two stemmed topic tokens that distinguish
the clause from the other numeric clauses. Words shared by every clause do
not count toward this guard. The two-token threshold rejects a single
incidental match, but it remains a lexical guard rather than semantic proof.
The earliest witness for each group joins the result in ranked order. Joint
witnesses come only from the first `limit` provenance-eligible hits, and that
page must cover every group, or the old empty per-entry result remains. Role and negation constraints never combine
across entries. A clause structure with singleton, overlapping, or uncovered
numbers is ambiguous and keeps that same fallback.

The gate filters ordered hits after the provenance filter and before `limit`.
Strict answer queries exclude the standing and expansion lanes so an
off-topic neighbor cannot satisfy a constraint. Ordinary recall keeps both
lanes. Coverage, scores, and ordering are unchanged. The two flags compose.
Abstention therefore has two causes behind one signal: zero verified
coverage under `require_verified`, or no surviving hit under
`require_evidence` at a nonzero limit. The CLI prints a constraint verdict
only when the gate filtered a hit. An empty page at `limit` 0 is the caller's
choice, never an answer-binding miss. No model judges eligibility.

Hybrid mode (`--semantic` on CLI, `semantic: true` on MCP) fuses keyword
ranks with embedding cosine ranks via reciprocal rank fusion (k = 60) over
the same temporal pool, coverage, and ordering rule. A candidate is
topic-matched on a keyword hit or cosine ≥ 0.45. The `embed` cargo feature
ships a local embedder (bge-small-en-v1.5, ~100MB into `.cache` on first
use); without it, semantic requests error. Vector cache keyed by
`(model, sha256(normalized text))` — content-addressed, immune to mtime
skew. Scoring is O(N) cosine per query; ~10k entries is the practical
ceiling before a real index is worth its corruption modes.

### Standing constraints

A deterministic cue tagger marks entries as standing rules or preferences
(`always`, `never`, `from now on`, `I prefer`, `don't`, `must not`,
`make sure`, `whenever`, and close cousins). The tagger is read-only: it
never writes, never verifies, and uses no model. The lane is a derived view
rebuilt from the canonical files on every query — markdown stays the store of
truth, and no second store is introduced.

Only **verified** entries are eligible: unverified and contradicted content
must not steer a task briefing (EXTRACTION-THREAT-MODEL.md). This is a read
filter, not a verification step. A query-shape router unions a capped slice of
the lane (at most 4 entries) into **task-shaped** queries only. A query is
task-shaped when it asks the agent to produce or act (`show me`, `write`,
`how do I`, `please`, and similar, including common single-word inflections
like `building`), with no fact cue. Fact questions — especially temporal and
knowledge-update queries (`when`, `did`, `current`, `moved to`, `no
longer`) — and every `as_of`/`current_only` query are fact-shaped and never
receive the lane, so generic rules cannot drop temporal hit@10. A fact cue
wins when both shapes appear; an unrecognized query shape defaults to
fact-shaped.

The lane **appends** into spare slots below `limit`, after the lexical hits
are ordered. When the cap binds, the slice is newest by the entry timestamp,
then descending EntryId as a deterministic tie-break
(ULIDs carry only millisecond time plus 80 random bits, so same-millisecond
entries are not ordered by creation). It never evicts or reorders a lexical
hit; when lexical hits fill the limit the union is empty. Lane entries carry
no lexical relevance (score 0.0) and do not count as topic-matched coverage,
so a briefing that lists standing constraints reports them instead of
claiming no knowledge matched. Cues match whole words in either the summary
or body, never a phrase assembled across the field boundary.
`require_verified` and `require_evidence` are strict queries and do not union
standing constraints. The same lane applies to ordinary keyword and hybrid
recall; `embed` stays opt-in.

### Entity and session expansion

Summarization and event-ordering probes returned only the turns that shared
question words (mean source coverage 0.09 and 0.06), so recall builds a
derived adjacency lane on each query. Keys come from the normalized
`entity_mentions()` strings (frontmatter `entities:` plus name-shaped
`[[wikilinks]]`) and from `session_id`. The two namespaces use separate key
prefixes so an entity string and a session id cannot collide. The map is
derived from the canonical entries in the temporal candidate pool on every
read. Nothing is persisted, and no parser, triple store, or model runs on the
write path.

Every ordered lexical or vector hit that is `Verified` at the query instant
and has positive relevance seeds one hop: entries that share a seed's entity
string or session id become candidates. Only neighbors evaluated `Verified`
at the query instant are eligible, so unverified, contradicted, and
rolled-back entries neither seed expansion nor enter the lane. Seeds and
any id already in the hit set or the standing lane are excluded.
`require_verified` stays purely lexical and returns no expansion.
`require_evidence` also returns no expansion because an off-topic neighbor
cannot serve as answer evidence. Other query shapes, including fact-shaped,
`current_only`, and `as_of`, expand from their own temporal pool.

At most `EXPANSION_LANE_CAP = 6` neighbors append after the ordered hits and
the standing lane, under `limit`. This is a second, independent budget; the
standing lane keeps its cap of 4. Expansion hits carry score 0.0 and do not
change topic-matched coverage or abstention. Lexical hits keep their order
and scores. The lane is deterministic: newest `timestamp` first, then
descending `EntryId`, then the cap. `as_of`, `current_only`, rollback, and
latest-slot resolution stay in force because the lane reads the same visible
candidate pool. Both keyword and opt-in hybrid recall call the shared
`finish`, so no second call site exists; `embed` stays opt-in and adds no
embedder synonym seed.

## Rollback (non-destructive)

No file is ever deleted.

1. Walk the `revises` chain backward from the target.
2. Find the first ancestor with `verification = verified`.
3. Mark every intermediate entry `rolled_back` (append a verification record).
4. Write a new `kind = rollback` entry whose `revises` points at the verified
   ancestor; summary records what was rolled back.

No verified ancestor ⇒ error, store untouched. Rolled-back entries stay on
disk; recall hides them by default.

## Conflict candidates

Writes that assert claims (`add`, `supersede`, `promote`, `end_session`
promotions) are checked against live verified facts after they land. A
plausible conflict records a candidate in `<root>/.conflicts/<id>.md` —
one markdown file per candidate, YAML frontmatter plus a rendered body —
and the write still succeeds. Candidates are advisory, never a gate.

A candidate records: new entry id, conflicting entry id, layer, summary
snapshots, the detection signal(s), a detection timestamp, and a status
(`open` → `resolved_superseded` | `resolved_contradicted` | `dismissed`,
with `resolved_by` / `resolved_at`).

Detection is deterministic: BM25-lite topic match under the `current`
temporal view, then lexical signals over stemmed summary tokens —
`shared_frame`, `negation_asymmetry`, `numeric_mismatch`. With the `embed`
build, `--semantic` adds `semantic_overlap` (cosine ≥ 0.70). Only live
verified, claim-shaped entries are candidates; the new entry, its
supersession parent, and its evidence refs are exempt.

A candidate must be an actionable disagreement, not two claims about the
same topic. `shared_frame` therefore requires both summaries to be
*focused* (at most 12 unique stemmed tokens each) and to swap a value
(each side carries a private token). A whole 200-char source-unit summary
is not a focused claim, so two distinct episodes that merely share
topical tokens do not raise `shared_frame`. Explicit value flips stay
actionable on any length: `numeric_mismatch` (different digit-bearing
tokens) and `negation_asymmetry` (one side negates) still qualify a long
claim. On those long pairs `shared_frame` is absent, so a reviewer can see
that the numeric or negation flip, not topical overlap, raised the
candidate. The opt-in `semantic_overlap` channel is unchanged. Candidates
remain advisory and unverified-by-birth; no `revises` link is written from
a candidate until a logged `resolve --supersede` decision.

Resolution reuses the trust verbs: `--supersede` links the new entry as
revision of the conflicting one, `--contradict` marks the verified entry
contradicted by the new claim, `--dismiss` closes the record. Briefings
warn on open candidates touching the topic the way they warn on
contradicted facts. `gungnir conflicts` lists the queue; `gungnir
contradict <id> --by <id>` exposes the underlying verb directly.

## Concurrency & durability

- Writes: temp file in the same directory + `rename` (atomic on POSIX & Windows).
- A store-level `flock` serializes multi-entry operations across processes.
- ULIDs make id collisions practically impossible; no retry loop needed.
- Crash mid-write leaves the previous version intact plus an orphaned
  `.tmp-*` sibling; startup sweep may delete stale temporaries.

## Audit surface

`git log`/`diff`/`blame` for belief — pure reads over the store; nothing
here writes.

- `gungnir log <id>` — the entry's verification records plus its revises
  lineage (ancestors above, successors below), citations, and entity
  mentions.
- `gungnir log [--since <t>] [--agent <a>] [--layer <l>]` — a chronological
  scan across every partition. Events are entry creations and each
  verification-log transition; `--agent` matches the actor (entry agent for
  creations, verifier for transitions). `--since` also applies to `log <id>`,
  filtering the records shown.
- `gungnir diff <t1> <t2> [--topic <q>] [--layer <l>]` — two snapshots
  evaluated through the same temporal machinery as `as_of` recall, diffed:
  added, verified, superseded, contradicted, rolled-back.
- `gungnir blame <id-or-topic>` — vouching (who verified/contradicted, when,
  with what note), what the belief revises, what replaced it, what cites it.
  A non-id argument resolves to the best topic match.
- `gungnir review [--layer <l>]` — the human ritual: belief-kind entries
  still `unverified`, grouped by partition then project, chain tails
  excluded, machine-extracted entries flagged, with the
  verify/supersede/contradict verbs printed per entry.
- `gungnir entities [name] [--layer <l>]` — the entity index: names with
  counts, or the entries under one name.

## Extraction

`gungnir extract` proposes durable facts from a transcript through a
configured backend — gated exactly as `docs/EXTRACTION-THREAT-MODEL.md`
specifies. Source is `--from <session-id-or-entry>`, `--file <path>`, or
`--text <string>` (exactly one); `--agent` is required.

- Backend: `GUNGNIR_EXTRACTOR` names a command honoring the JSON
  stdin/stdout contract in `docs/INTEGRATIONS.md`, or the literal
  `heuristic` for the shipped deterministic backend. Unset is a clean
  no-op, not an error.
- Gate, in order: strict schema, evidence citation (every `cites` index
  must resolve to a unit carrying ≥1 evidence link; `--allow-unbacked`
  relaxes this to an explicit unverified write), secret denylist (built-in,
  never empty, extended by `GUNGNIR_EXTRACT_DENYLIST`), instruction-shape
  refusal, per-run volume cap (`--cap`, default 20) plus dedupe against the
  journal.
- Accepted proposals land through the ordinary write path — journal-only,
  `unverified`, `source_tool: extract`, `extracted_by: <backend>` — and run
  conflict detection like any claim write. Nothing the extractor produces
  is ever born verified or written to the codex.
- Machine origin is visible everywhere an agent reads: `ls --unreviewed`
  and `review` flag extracted entries (`[extracted]`), `get`/`log`/`blame`
  render `origin: extract via <backend>`, briefings tag hits `extracted`,
  and recall lines carry `[extracted]` on CLI and MCP.

## Surfaces

- **Library** (`gungnir` crate): everything above.
- **CLI** (`gungnir` bin): `init`, `add`, `get`, `ls`, `extract`, `recall`, `verify`
  (`--evidence` records file hashes at verify time; `--revalidate` re-hashes
  and flags drift, `--all` sweeps the store), `contradict`, `supersede`,
  `rollback`, `promote`, `brief`, `conflicts`, `log`, `diff`, `blame`,
  `review`, `entities`, `session`, `stats`
  (`--topic` reports cross-journal coverage counts), `mcp`. Exit codes: 0
  success, 1 error, 2 explicit abstention (`--require-verified` provenance,
  `--require-evidence` answer binding).
- **MCP server**: tools `start_session`, `end_session`, `add_observation`,
  `add_attempt`, `extract`, `recall`, `brief`, `verify`, `contradict`,
  `promote`, `supersede`, `rollback`, `conflicts`, `resolve_conflict`,
  `stats`, `revalidate`, `list`, `get`.

## Deliberate departures from the reference design

- ULID ids (time-sortable, 128-bit) vs 48-bit random hex
- `flock` cross-process serialization vs none
- content-hash-keyed embedding cache vs `(mtime, size)` keying
- day-level directory sharding vs month-level
- independent naming/domain vocabulary throughout
