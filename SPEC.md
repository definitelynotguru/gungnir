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
final_score)` where bucket orders verified > unverified > contradicted
and `final_score = relevance × trust × recency` — trust weights the bucket
(1.0 / 0.75 / 0.35) and recency decays at a 90-day half-life floored at
0.25. Rolled-back entries are always hidden.

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

Hybrid mode (`--semantic` on CLI, `semantic: true` on MCP) fuses keyword
ranks with embedding cosine ranks via reciprocal rank fusion (k = 60) over
the same temporal pool, coverage, and ordering rule. A candidate is
topic-matched on a keyword hit or cosine ≥ 0.45. The `embed` cargo feature
ships a local embedder (bge-small-en-v1.5, ~100MB into `.cache` on first
use); without it, semantic requests error. Vector cache keyed by
`(model, sha256(normalized text))` — content-addressed, immune to mtime
skew. Scoring is O(N) cosine per query; ~10k entries is the practical
ceiling before a real index is worth its corruption modes.

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
- `ls --unreviewed` and `review` flag extracted entries (`[extracted]`);
  `log`/`blame`/`show` render `origin: extract via <backend>`.

## Surfaces

- **Library** (`gungnir` crate): everything above.
- **CLI** (`gungnir` bin): `init`, `add`, `get`, `ls`, `extract`, `recall`, `verify`
  (`--evidence` records file hashes at verify time; `--revalidate` re-hashes
  and flags drift, `--all` sweeps the store), `contradict`, `supersede`,
  `rollback`, `promote`, `brief`, `conflicts`, `log`, `diff`, `blame`,
  `review`, `entities`, `session`, `stats`
  (`--topic` reports cross-journal coverage counts), `mcp`. Exit codes: 0
  success, 1 error, 2 explicit abstention under `--require-verified`.
- **MCP server**: tools `start_session`, `end_session`, `add_observation`,
  `add_attempt`, `extract`, `recall`, `brief`, `verify`, `contradict`,
  `promote`, `supersede`, `rollback`, `conflicts`, `resolve_conflict`,
  `list`, `get`.

## Deliberate departures from the reference design

- ULID ids (time-sortable, 128-bit) vs 48-bit random hex
- `flock` cross-process serialization vs none
- content-hash-keyed embedding cache vs `(mtime, size)` keying
- day-level directory sharding vs month-level
- independent naming/domain vocabulary throughout
