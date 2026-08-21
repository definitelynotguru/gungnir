# Integrations

Gungnir reaches agents through three surfaces: the MCP server, the CLI, and
the Rust library. All three operate on the same on-disk store, so mixing them
is safe; every write goes through the same validation and locking.

## MCP server (Claude Code, Cursor, Cline, ...)

The server speaks newline-delimited JSON-RPC over stdio per the Model Context
Protocol. Tools: `start_session`, `add_observation`, `add_attempt`,
`end_session`, `recall`, `brief`, `verify`, `get`.

Build first:

```bash
cargo install --path .   # or cargo build --release and use target/release/gungnir
```

### Claude Code

```bash
claude mcp add gungnir --env GUNGNIR_ROOT=$HOME/gungnir -- gungnir mcp
```

### Cursor

`.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "gungnir": {
      "command": "gungnir",
      "args": ["mcp"],
      "env": { "GUNGNIR_ROOT": "/home/you/gungnir" }
    }
  }
}
```

### Generic client

Any MCP-capable host works with the same shape: command `gungnir`, argument
`mcp`, environment `GUNGNIR_ROOT` pointing at a directory.

Recommended agent workflow once connected:

1. Call `start_session` with your agent name and task. The result carries a
   session id plus a briefing of relevant Codex facts and your own prior
   attempts.
2. Record observations and attempts as you work.
3. Call `end_session` with a one-line summary when done. Scratch clears,
   the transcript archives to your private journal.
4. When a finding deserves sharing, promote it through the CLI (below) so it
   lands in the Codex with provenance linked to its journal source.

## CLI

The CLI drives the same store. Useful for humans, cron jobs, and agent hooks.

```bash
export GUNGNIR_ROOT=~/gungnir        # or pass --root <path> globally

gungnir init                          # create the directory structure
gungnir session start --agent builder "task text"
gungnir session obs <sid> --agent builder "observation text"
gungnir session attempt <sid> --agent builder "what was tried" --ok
gungnir session end <sid> --agent builder --summary "one line"

gungnir add --agent builder --kind decision --summary "use postgres" --body "why..."
gungnir recall "checkout query" --limit 5
gungnir recall "prior attempts" --layer journal --agent builder
gungnir brief --agent builder "slow checkout"
gungnir verify <id> --verifier team-review --note "confirmed in PR 42"
gungnir supersede <id> --agent builder --summary "revised conclusion"
gungnir rollback <id> --agent builder
gungnir promote <journal-entry-id> --agent builder --kind decision \
  --summary "shared finding" --body "details"
```

## Rust library

```toml
[dependencies]
gungnir = { version = "0.1" }        # crates.io release pending;
                                     # git = "https://github.com/definitelynotguru/gungnir"
```

```rust
use gungnir::{EntryKind, Gungnir, Promotion};

let g = Gungnir::open("~/gungnir")?;

let s = g.start_session("builder", "fix slow checkout query");
g.add_observation(&s, "EXPLAIN shows seq scan")?;
g.add_attempt(&s, "index hint", false)?;
let report = g.end_session(&s, "rewrote checkout query", vec![
    Promotion {
        kind: EntryKind::Decision,
        summary: "checkout must use orders_archive_idx".into(),
        body: "seq scan was the bottleneck".into(),
    },
])?;

let briefing = g.brief("builder", "slow checkout", 8)?;
print!("{}", briefing.markdown);
```

A runnable version ships as an example:

```bash
cargo run --example quickstart
```

## Embeddings (optional)

The default build does keyword recall only and makes zero network calls. To
enable hybrid vector + keyword search, implement the `Embedder` trait against
your provider and wrap it in the content-addressed cache:

```rust
use gungnir::embedding::{CachedEmbedder, Embedder};
use gungnir::recall::Query;

struct MyClient { endpoint: String, model: String }

impl Embedder for MyClient {
    fn model(&self) -> &str { &self.model }
    fn embed(&self, texts: &[String]) -> gungnir::Result<Vec<Vec<f32>>> {
        // POST texts to your embedding endpoint here, return vectors in order.
        # todo!()
    }
}

let cached = CachedEmbedder::new(MyClient { /* ... */ }, "~/gungnir");
let hits = gungnir::embedding::hybrid_search(
    &g.codex()?, &Query::new("slow checkout", 10), &cached)?;
```

Vectors cache under `<root>/.cache/embeddings/<model>/` keyed by
`sha256(model + normalized text)`. Deleting `.cache` is always safe; it
rebuilds on demand.

## Versioning your memory with git

The whole root is plain markdown. Recommended `.gitignore` inside the memory
root when only shared truth should be versioned:

```
scratch/
journal/
.cache/
```

Commit everything except `.cache/` if agent journals belong in history too.
Rollback and supersession chains read as ordinary commit diffs.
