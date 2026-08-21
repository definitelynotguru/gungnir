# Gungnir build plan

Local-first, markdown-native memory for AI agents. Original Rust. Behavioral
spec lives in SPEC.md.

## Phases

Each phase ends with `cargo test` green and `cargo clippy` silent before the
next begins.

1. **Rename** crate `gungnir` (lib + bin). Done.
2. **Layout + sessions + briefing.** `src/layout.rs` (root resolution,
   dir names, component sanitization), `EntryKind::Session`, `src/briefing.rs`
   (pure compiler), facade skeleton in `src/gungnir.rs`.
3. **Recall.** `src/recall.rs`: tokenizer with stopwords, verification-bucket
   ordering (verified > unverified > contradicted; rolled-back hidden),
   score = weighted token overlap over summary (2x) and body (1x).
4. **Rollback.** `src/rollback.rs`: plan/apply split, walk `revises` chain to
   first verified ancestor, mark intermediates rolled back, write rollback
   entry. Cycle-guarded.
5. **Facade.** `src/gungnir.rs`: sessions (start/add/end), promotion to Codex
   with evidence links back to Journal, briefing assembly, supersede,
   verify. Scratch directory removed after successful archive.
6. **CLI.** clap derive: init, add, get, ls, recall, verify, supersede,
   rollback, promote, brief, session start/end. Runtime smoke on real binary.
7. **MCP server.** Hand-rolled newline-delimited JSON-RPC over stdio (no
   tokio, no SDK churn). Tools: start_session, add_observation, add_attempt,
   end_session, recall, brief, verify, get. Runtime proof: subprocess
   handshake test via CARGO_BIN_EXE.
8. **Embeddings.** `Embedder` trait, content-hash disk cache, cosine, RRF
   fusion (k=60), hybrid search. Fake embedder in tests; no HTTP dep.
9. **README + final verify.**

## Alternatives considered

- Layer partitioning: frontmatter field vs directories. Chose directories:
  privacy becomes a gitignore line, scratch cleanup is rm -r, Store reuse.
- MCP: rmcp SDK vs hand-rolled stdio loop. Chose hand-rolled: ~200 lines,
  zero async runtime, immune to SDK API churn.
- Embeddings: bundled HTTP client vs pluggable trait. Chose pluggable: lean
  default build, users wire their own endpoint.

## Verification

Static: `cargo test`, `cargo clippy --all-targets` per phase.
Runtime: real CLI invocations against a temp root; MCP subprocess handshake;
full workflow (session → observations → end → promote → brief → rollback)
executed through the CLI binary, not just unit tests.
