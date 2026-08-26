# Changelog

All notable changes to Gungnir are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [0.2.0] - 2026-08-26

### Added

- **Temporal recall.** `Query` gains `as_of` and `current_only` (builder
  methods `.as_of(ts)` / `.current()`). Point-in-time evaluation derives
  verification state from the append-only verification log, so "what was true
  at T" needs no schema change. Current-only mode resolves revises chains to
  their heads and drops contradicted facts.
- **Coverage and abstention.** Search results carry per-topic coverage counts
  (verified / unverified / contradicted, plus hidden superseded and rolled
  back). Briefings print a coverage section and state plainly when no
  verified knowledge covers the task. CLI: `recall --current`,
  `recall --as-of <RFC3339>`.
- **Gungnir Bench** (`tests/bench.rs`): deterministic accuracy harness with
  sixteen checks across five LongMemEval-style abilities — information
  extraction, multi-session reasoning, knowledge updates, temporal reasoning,
  abstention. Offline exact grading; per-ability scores print with
  `--nocapture`.
- **MCP parity.** Four new tools over the stdio server: `promote`,
  `supersede`, `rollback`, `list`. Twelve tools total; the subprocess test
  drives the full correction lifecycle over the wire.
- **`gungnir stats`**: memory health summary — counts by layer and
  verification state, superseded depth, entries stale over 30 days,
  verification rate. `--json` flag for tooling; `--agent` scopes journals.
- **CI**: GitHub Actions running fmt check, clippy `-D warnings`, tests, and
  cargo-audit on Linux/macOS/Windows.
- **docs/EXTRACTION-THREAT-MODEL.md**: design gate for the future
  auto-extraction feature. Threat model and seven required mitigations;
  extraction stays unimplemented until they ship.

### Changed

- rustfmt applied across the tree.
- `briefing::compile` now takes a `BriefingInput` struct instead of positional
  parameters.

### Breaking

- Struct-literal construction of `Query` outside the crate gains two new
  fields; use `Query::new(...)` (unchanged) or set them explicitly.

## [0.1.0] - 2026-08-21

Initial release.

### Added

- Three-layer memory: Scratch (per-task, ephemeral), Journal (per-agent,
  private), Codex (shared truth), as directory partitions of one markdown
  store.
- Session lifecycle: `start_session`, observations, attempts with outcomes,
  `end_session` archiving transcripts into the Journal and clearing scratch.
- Pre-task briefings combining Codex facts with the requesting agent's own
  Journal history, with `[verified]`, `[superseded]`, and `[CONTRADICTED]`
  markers plus relevant transcript excerpts.
- Provenance model: file evidence with SHA-256 and excerpt caps, cross-layer
  entry references, append-only verification log, contradiction links.
- Supersession chains via `revises`; non-destructive rollback walking to the
  first verified ancestor with cycle detection.
- Keyword recall with verification-bucket ordering; rolled-back entries hidden
  by default.
- Optional hybrid recall: pluggable `Embedder` trait, content-addressed disk
  cache (`sha256(model + normalized text)`), reciprocal rank fusion (k = 60).
- CLI covering the full lifecycle: `init`, `add`, `get`, `ls`, `recall`,
  `verify`, `supersede`, `rollback`, `promote`, `brief`,
  `session start|obs|attempt|end`.
- MCP server over stdio (newline-delimited JSON-RPC 2.0) exposing eight tools,
  verified against the real binary in a subprocess test.
- Durability guarantees: atomic temp-plus-rename writes, cross-process `flock`
  serialization, stale-temporary sweep on open.
- ULID-based identity: time-sortable filenames, deterministic date sharding,
  no collision-retry paths.

[0.2.0]: https://github.com/definitelynotguru/gungnir/releases/tag/v0.2.0
[0.1.0]: https://github.com/definitelynotguru/gungnir/releases/tag/v0.1.0
