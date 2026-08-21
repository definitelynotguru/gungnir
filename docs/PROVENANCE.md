# Provenance

Every fact in Gungnir answers three questions: where did this come from, who
vouches for it, and what replaced it. This document describes the machinery
behind those answers and the trust boundaries between memory layers.

## Evidence

Entries can carry evidence links, validated at write time.

```yaml
evidence:
  - kind: file
    path: docs/adr/0007-database-choice.md
    excerpt: "We will use PostgreSQL because..."
    sha256: 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
  - kind: ref
    id: 01JZZZZZZZZZZZZZZZZZZZZZZZ
```

- **file** points at a filesystem artifact, with an excerpt capped at 500
  characters and a SHA-256 of the referenced content at citation time.
- **ref** points at another entry, in any layer. A Codex fact may cite the
  Journal archive entry it emerged from. Reference resolution crosses layer
  boundaries by design; provenance chains do not stop at partition edges.

Dangling references fail the write. Evidence cannot point at nothing.

## Verification

Verification is a state machine. Entries are born `unverified`, and the type
system makes every other state reachable only through a logged transition.

```
unverified ──verify()──▶ verified
unverified ──contradict()──▶ contradicted { by }
any ────────mark_rolled_back()──▶ rolled_back
```

Each transition appends a `verification_log` record: who, when, what, and an
optional note. The log is append-only within an entry's life. Recall orders by
verification bucket, so verified facts outrank hearsay at equal relevance and
contradicted facts sink below unverified ones while staying visible enough to
warn about. Briefings print `[CONTRADICTED]` tags and a warning line whenever
a contradicted fact makes the cut.

## Supersession

Facts change. Gungnir never overwrites one; it writes a revision linked by
`revises` to its predecessor.

```
v1 (verified) ◀── revises ── v2 ◀── revises ── v3
```

Any entry whose id appears in some other entry's `revises` field is flagged
`[superseded]` in briefings. The old fact remains on disk, fully readable,
with its own provenance intact. History is never edited.

## Rollback

Rollback is the inverse of supersession and equally non-destructive. Walking
the chain backward from the target, Gungnir finds the first verified ancestor,
marks every intermediate `rolled_back`, and writes a rollback entry revising
that ancestor. Rolled-back entries vanish from recall but remain on disk for
audit. See [DESIGN.md](DESIGN.md#rollback) for the full contract.

## Trust boundaries between layers

| Layer   | Path                  | Who reads it            | Lifetime        |
|---------|-----------------------|-------------------------|-----------------|
| Scratch | `scratch/<session>/`  | the owning session      | cleared at end  |
| Journal | `journal/<agent>/`    | the owning agent        | permanent       |
| Codex   | `codex/`              | everyone                | permanent       |

Privacy is enforced by directory partitioning rather than runtime checks.
Briefings assemble Codex hits plus the requesting agent's Journal hits only;
another agent's failures never leak into your context. This property is covered
by test: agent B's journal entries do not appear in agent A's briefing.

What lands in the Codex is a deliberate act (`promote`), linked back to its
Journal source by an evidence ref. The shared truth carries its lineage with
it.

## What provenance does not claim

- File evidence hashes content at citation time; it does not detect later
  edits to the cited file. Diff your repo if you need that guarantee.
- Verification records name a verifier string; Gungnir does not authenticate
  humans. "verified" means someone took responsibility, recorded under their
  name.
- Rolled-back does not mean wrong. It means the chain restored an earlier
  state; read the rollback entry's body for the list of what was undone.
