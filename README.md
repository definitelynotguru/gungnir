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

# Mark it verified once a human confirms.
gungnir verify $CID --verifier team-review --note "confirmed in PR 42"
```

The next agent that runs `session start` on a related task gets that fact in
its briefing, tagged `[verified]`. Failed attempts stay private to the agent
that tried them.

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
  machine, never a free field.
- **Supersession, not overwrite.** Revisions chain via `revises`; briefings
  flag superseded facts instead of silently showing stale ones.
- **Non-destructive rollback.** Rolling back walks the revision chain to the
  first verified ancestor, marks intermediates rolled back, and writes a
  rollback entry. No file is ever deleted.
- **Recall you can reason about.** Keyword scoring over summary (2x) and body,
  ordered by verification bucket: verified > unverified > contradicted;
  rolled-back entries hidden. Hybrid vector+keyword fusion via reciprocal rank
  fusion is available by plugging in an embedder (`src/embedding.rs`).

## Library

```rust
use gungnir::{EntryKind, Gungnir, Promotion};

let g = Gungnir::open("~/gungnir")?;
let s = g.start_session("builder", "fix slow checkout query");
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
`recall`, `brief`, `verify`, `get`.

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
| [CHANGELOG.md](CHANGELOG.md)      | Release history                                 |
| [examples/quickstart.rs](examples/quickstart.rs) | Runnable end-to-end tour          |

## Status

Early but tested: 48 tests including an MCP subprocess handshake against the
real binary. API may still move before 1.0. See
[CHANGELOG.md](CHANGELOG.md) for what shipped and when.

## License

MIT. See [LICENSE](LICENSE).
