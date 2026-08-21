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

Default mode is keyword scoring. Query tokens (stopwords stripped) match
against summary and body; summary hits weigh double. Filtering happens before
scoring. Results sort by verification bucket first, score second:
verified outranks unverified at equal relevance, contradicted sinks,
rolled-back entries disappear entirely.

Hybrid mode fuses keyword ranks with embedding cosine ranks through reciprocal
rank fusion (k = 60). The fusion function is pure and unit-tested against a
fake embedder; no network code ships in the default build.

The embedding cache is content-addressed: `sha256(model + "\n" +
normalized_text)`. Editing a file invalidates its vector naturally. Switching
models partitions the cache by directory. Nothing keys on mtime, because
mtimes lie under git operations, sync tools, and restores.

## Rollback

Non-destructive by contract. Rolling back a target walks the `revises` chain
backward to the first verified ancestor, marks every intermediate rolled back
by appending a log record, and writes a new rollback entry pointing at that
ancestor. No file is ever deleted or rewritten destructively. A missing
verified ancestor aborts the plan before any mutation. Cycle detection guards
against corrupted chains.

Plan and apply are separate functions, so tooling can preview a rollback
before executing it.

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
