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
| `verification_log`| list of records     | `{verifier, timestamp, status, note}`   |
| `evidence`        | list                | `{kind: file, path, excerpt ≤ 500, sha256}` or `{kind: ref, id}` (refs must exist) |
| `source_tool`     | string?             |                                         |

Body: free-form markdown after the closing `---`.

## Write-time validation

- id unique within store
- every referenced id (`revises`, `review_of`, `evidence[].ref`,
  `contradicted_by`) resolves to an existing entry
- `open` ⇒ `assigned_to` present
- `kind = review` ⇒ `review_of` present
- entries are born `unverified`; `verified` reachable only via `verify()`
- length caps enforced (summary 200, excerpt 500)

## Recall

Default mode: keyword scoring — token overlap across `summary` + body,
case-folded, split on non-alphanumerics. Filtering happens before scoring.
Sort key: `(verification_bucket, score)` where bucket orders
verified > unverified > {contradicted, rolled_back}; contradicted and
rolled_back are excluded unless explicitly requested.

Hybrid mode (feature flag `embeddings`): fuse keyword ranks with embedding
cosine ranks via reciprocal rank fusion (k = 60). Vector cache keyed by
`(model, sha256(normalized text))` — content-addressed, immune to mtime skew.

## Rollback (non-destructive)

No file is ever deleted.

1. Walk the `revises` chain backward from the target.
2. Find the first ancestor with `verification = verified`.
3. Mark every intermediate entry `rolled_back` (append a verification record).
4. Write a new `kind = rollback` entry whose `revises` points at the verified
   ancestor; summary records what was rolled back.

No verified ancestor ⇒ error, store untouched. Rolled-back entries stay on
disk; recall hides them by default.

## Concurrency & durability

- Writes: temp file in the same directory + `rename` (atomic on POSIX & Windows).
- A store-level `flock` serializes multi-entry operations across processes.
- ULIDs make id collisions practically impossible; no retry loop needed.
- Crash mid-write leaves the previous version intact plus an orphaned
  `.tmp-*` sibling; startup sweep may delete stale temporaries.

## Surfaces

- **Library** (`gungnir` crate): everything above.
- **CLI** (`mnem` bin): `init`, `add`, `get`, `ls`, `recall`, `verify`,
  `supersede`, `rollback`, `brief`.
- **MCP server** (milestone 3): tools `start_session`, `end_session`,
  `add_observation`, `add_attempt`, `recall`, `verify`.

## Deliberate departures from the reference design

- ULID ids (time-sortable, 128-bit) vs 48-bit random hex
- `flock` cross-process serialization vs none
- content-hash-keyed embedding cache vs `(mtime, size)` keying
- day-level directory sharding vs month-level
- independent naming/domain vocabulary throughout
