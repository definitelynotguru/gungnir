# Gungnir

Local-first, markdown-native memory for AI agents. One spear, never misses:
agents stop re-using stale facts, repeating failed attempts, and contradicting
decisions made last month.

## Why

AI coding agents start every task from zero. They re-run fixes that already
failed. They suggest patterns the team migrated away from. Nobody can audit
what the agent "knew" when it acted.

Gungnir gives agents a shared, versioned source of truth on your own disk.
Every fact carries provenance. Superseded facts are marked, never overwritten.
Before each task the agent gets a briefing: what's current, what's superseded,
what already failed.

## Quick start

```bash
cargo install --path .
export GUNGNIR_ROOT=~/gungnir   # optional, defaults to ~/.gungnir
gungnir init
```

A full session through the CLI:

```bash
# Open a working session; prints a session id plus your briefing.
SID=$(gungnir session start --agent builder "fix slow checkout query")

# Work. Record what you see and try.
gungnir session obs $SID --agent builder "EXPLAIN shows seq scan on orders_archive"
gungnir session attempt $SID --agent builder "added index on orders.user_id"
gungnir session attempt $SID --agent builder "rewrote query to use orders_archive_idx" --ok

# Archive into your private journal and clear scratch.
gungnir session end $SID --agent builder --summary "rewrote checkout query"

# Promote the durable finding into the shared codex, linked to its source.
JID=$(find $GUNGNIR_ROOT/journal/builder -name '*.md' | head -1)
CID=$(gungnir promote ${JID%.md} --agent builder --kind decision \
  --summary "checkout queries must use orders_archive_idx")

# Mark it verified once a human confirms, citing the ADR's content hash.
gungnir verify $CID --verifier team-review --note "confirmed in PR 42" \
  --evidence docs/adr/0003-orders-index.md

# Later, prove the cited file still says what it said.
gungnir verify $CID --revalidate        # or --all for every entry

# What do we believe right now, and how healthy is the memory?
gungnir recall "checkout" --current
gungnir stats
```

The next agent that runs `session start` on a related task gets that fact in
its briefing, tagged `[verified]`, with a coverage line confirming verified
knowledge exists. Failed attempts stay private to the agent that tried them.
Point-in-time questions work too:

```bash
gungnir recall "deploys" --as-of 2026-08-01T00:00:00Z
```

## Semantic recall (opt-in)

The default build is BM25-lite keyword scoring with stemming and makes
zero network calls. Embedding similarity is an opt-in cargo feature named
`embed` — it is never part of `default`, and nothing in a default install
pulls model weights:

```bash
cargo install --path . --features embed   # or: cargo build --features embed
```

That builds in a local bge-small-en-v1.5 embedder (quantized, ~100MB via
fastembed/ONNX). Nothing downloads at build time or server start: weights
fetch lazily on the first semantic query into `<root>/.cache/fastembed` —
the only network call gungnir ever makes — and derived vectors cache under
`<root>/.cache/embeddings/` keyed by content hash.

```bash
gungnir recall "cache eviction" --semantic
gungnir brief --agent builder "cache eviction" --semantic
# MCP: "semantic": true on the recall / brief / extract tools
```

Ask for semantic on a build without the feature and gungnir errors
("semantic recall requires a build with `--features embed`") instead of
silently answering with keyword results. To hybrid search against your own
embedder instead of the shipped model, implement the `Embedder` trait —
see [docs/INTEGRATIONS.md](docs/INTEGRATIONS.md).

## Auditing what it believes

Gungnir is `git log`/`diff`/`blame` for belief — the same versioning story
the store keeps internally, readable daily:

```bash
gungnir log <id>                        # verification log + revises lineage
gungnir log --since 2026-10-01T00:00:00Z [--agent builder] [--layer codex]
gungnir diff 2026-09-01T00:00:00Z 2026-10-01T00:00:00Z [--topic checkout]
gungnir blame <id-or-topic>             # who vouched, what it revises, what replaced it
gungnir review                          # unverified beliefs grouped for triage
gungnir entities [name]                 # entity index: names+counts, or entries under a name
```

All of them are pure reads over the store across every partition (codex,
each journal, scratch); `--layer` narrows the scan. Entries can carry
`entities:` names in frontmatter and `[[id]]` wikilinks or `[[name]]`
mentions in the body — id links are validated like evidence refs, and
supersede inherits the parent's entity list.

## Abstaining on purpose

The coverage claim is machine-readable, not just a prose line:

```bash
gungnir recall "postgres" --require-verified   # verified hits only
echo $?                                        # 2 = explicit "no verified knowledge"
gungnir recall "postgres" --require-verified --json   # {hits, coverage, abstained}
gungnir brief --agent builder "postgres" --require-verified
```

Exit codes: 0 success, 1 error, 2 abstention. On MCP, `recall` accepts
`require_verified` and `as_of` and always ends its result with a `coverage:`
JSON line including `abstained`; `brief` accepts `require_verified` and
says "N unverified or contradicted match(es) exist, none verified" when the
verdict fires.

## How it works

Three layers under one root:

| Layer     | Path                    | Visibility | Lifetime          |
|-----------|-------------------------|------------|-------------------|
| Scratch   | `scratch/<session>/`    | private    | cleared at end    |
| Journal   | `journal/<agent>/`      | per-agent  | permanent         |
| Codex     | `codex/`                | shared     | permanent         |

Entries are markdown files with YAML frontmatter, date-sharded `YYYY/MM/DD`,
one ULID-named file each. `git add` the whole root and your agent memory
version-controls alongside your code. Add `scratch/` and `journal/` to
`.gitignore` if only the shared codex should be committed.

Key behaviors:

- **Provenance first.** Evidence links (file excerpts with SHA-256, or other
  entries) are validated at write time. Verification is a logged state
  machine, never a free field. `verify --revalidate` re-hashes evidence
  files and flags drift: a fact whose cited ADR changed underneath it is
  not silently stale.
- **Supersession, not overwrite.** Revisions chain via `revises`; briefings
  flag superseded facts instead of silently showing stale ones.
- **Non-destructive rollback.** Rolling back walks the revision chain to the
  first verified ancestor, marks intermediates rolled back, and writes a
  rollback entry. No file is ever deleted.
- **Cross-agent coverage without leaks.** Briefings and `stats --topic`
  report aggregate counts over other agents' journals (how many attempted
  the topic, how many failed) so a second agent stops repeating a
  documented failure. Counts only: no peer summary or body ever leaves its
  journal. Topic existence does leak; see docs/PROVENANCE.md.
- **Conflicts surface, not hide.** A write that disagrees with a live
  verified fact still lands, and records a conflict candidate in
  `.conflicts/`. `gungnir conflicts` lists them;
  `gungnir conflicts resolve <id> --supersede|--contradict|--dismiss`
  closes the loop through the same logged verbs. Briefings warn while a
  candidate on the topic stays open. Over MCP, `conflicts` and
  `resolve_conflict` take `agent` and only reach candidates visible to
  that agent — shared-codex endpoints plus the agent's own journal — so
  another agent's private claim text never crosses the boundary.
- **Gated auto-extraction.** `gungnir extract` lets a configured backend
  (`GUNGNIR_EXTRACTOR`: a command honoring a JSON stdin/stdout contract, or
  the shipped `heuristic`) propose durable facts from a session archive or
  text — through the same write path as manual `add`, behind the mitigations
  in docs/EXTRACTION-THREAT-MODEL.md: evidence citation, secret denylist,
  instruction-refusal, volume caps, journal-only `unverified` writes marked
  `extracted_by`. No backend configured is a clean no-op.
- **Recall you can reason about.** Keyword scoring over summary (2x) and body,
  ordered by verification bucket: verified > unverified > contradicted;
  rolled-back entries hidden. `--current` resolves supersession chains to
  their heads and drops contradicted facts. `--as-of <RFC3339>` evaluates
  history from the verification log, no schema change required. Every search
  reports coverage, and briefings say plainly when no verified knowledge
  covers a task. Hybrid vector+keyword fusion via reciprocal rank fusion is
  available per query through the opt-in `embed` build — see
  [Semantic recall (opt-in)](#semantic-recall-opt-in).

## Library

```rust
use gungnir::{EntryKind, Gungnir, Promotion};

let g = Gungnir::open("~/gungnir")?;
let s = g.start_session("builder", "fix slow checkout query")?;
g.add_observation(&s, "seq scan on orders_archive")?;
g.add_attempt(&s, "index hint", false)?;
let report = g.end_session(&s, "rewrote checkout query", vec![
    Promotion {
        kind: EntryKind::Decision,
        summary: "checkout must use orders_archive_idx".into(),
        body: "seq scan was the bottleneck".into(),
    },
])?;
let briefing = g.brief("builder", "slow checkout", 8)?;
println!("{}", briefing.markdown);
```

A runnable version ships as an example:

```bash
cargo run --example quickstart
```

## MCP server

Works with Claude Code, Cursor, Cline, or anything speaking the Model Context
Protocol:

```json
{
  "mcpServers": {
    "gungnir": {
      "command": "gungnir",
      "args": ["mcp"],
      "env": { "GUNGNIR_ROOT": "/path/to/root" }
    }
  }
}
```

Tools: `start_session`, `add_observation`, `add_attempt`, `end_session`,
`recall`, `brief`, `verify`, `contradict`, `promote`, `supersede`,
`rollback`, `conflicts`, `resolve_conflict`, `extract`, `stats`,
`revalidate`, `list`, `get`. Agents drive
the full correction lifecycle over the wire.

## Design notes

- Atomic writes (temp + rename) and cross-process `flock` serialization.
  A crash leaves the previous version intact.
- ULID ids: time-sortable filenames, no collision-retry loops.
- Embedding cache is content-addressed (`sha256(model + normalized text)`),
  immune to mtime skew; bring your own HTTP client via the `Embedder` trait.
- No network calls anywhere in the default build.

## Documentation

| Document                          | Contents                                        |
|-----------------------------------|-------------------------------------------------|
| [SPEC.md](SPEC.md)                | Behavioral contract: layout, entry shape, rules |
| [docs/DESIGN.md](docs/DESIGN.md)  | Why it is built this way                        |
| [docs/PROVENANCE.md](docs/PROVENANCE.md) | Evidence, verification, supersession, trust boundaries |
| [docs/INTEGRATIONS.md](docs/INTEGRATIONS.md) | Claude Code / Cursor / Cline setup, library usage, embeddings |
| [docs/EXTRACTION-THREAT-MODEL.md](docs/EXTRACTION-THREAT-MODEL.md) | Design gate for future auto-extraction |
| [CHANGELOG.md](CHANGELOG.md)      | Release history                                 |
| [examples/quickstart.rs](examples/quickstart.rs) | Runnable end-to-end tour          |

## gungnir-bench

[`bench/`](bench/README.md) is a public, deterministic accuracy harness:
fixed corpus, 19 exact-match checks across five abilities, no LLM judge.
Any memory store can run it. The corpus and checks are YAML data, the
reference runner is `tests/bench.rs`, and CI keeps the artifact in sync
with what gungnir actually scores.

## Status

Early but tested: 92 tests including the gungnir-bench harness across five
abilities plus a paraphrase-recall suite, and an MCP subprocess handshake
driving the full correction lifecycle against the real binary. API may
still move before 1.0. See [CHANGELOG.md](CHANGELOG.md) for what shipped
and when.

## License

MIT. See [LICENSE](LICENSE).
