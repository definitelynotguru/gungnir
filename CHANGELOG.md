# Changelog

All notable changes to Gungnir are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

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

[0.1.0]: https://github.com/definitelynotguru/gungnir/releases/tag/v0.1.0
