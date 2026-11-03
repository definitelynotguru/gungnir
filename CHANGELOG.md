# Changelog

All notable changes to Gungnir are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **`contradict` on every surface.** New CLI subcommand
  `gungnir contradict <id> --by <id> [--verifier] [--note]` and matching MCP
  tool. `VerificationState::Contradicted` was previously unreachable outside
  the Rust API; the coverage counts and briefing warnings it feeds are now
  load-bearing.
- **`add --review-of <id>`** so `--kind review` entries can name their
  target from the CLI. Promoting with `kind=review` sets `review_of` to the
  source entry.
- **`Entry.outcome: bool`** for attempts. Session archives still render the
  `[succeeded]`/`[failed]` marker, now from a structured field instead of
  re-parsing the `outcome:` body line (which remains as legacy fallback).
- **Read resilience.** `Store::scan()` returns every parseable entry plus a
  `skipped` list of ULID-named files that failed to parse (torn writes, git
  conflict markers, hand edits). Recall, list, briefing, and stats report
  skips — stderr on the CLI, notes in MCP text results — instead of one
  corrupt file aborting the whole layer.
- **`Coverage.total`**: the count of entries in scope, so surfaces can
  distinguish "nothing exists" from "entries exist but none matched".
- **Session identity persistence.** `start_session` writes
  `scratch/<id>/.session.yaml`, so `end_session` archives the real task
  text even when the ending process only knows the session id.

### Changed (breaking)

- **`Gungnir::start_session` returns `Result<Session>`** instead of
  `Session` — persisting `.session.yaml` can fail.
- **`Gungnir::contradict` and `Entry::contradict` take a `note:
  Option<String>`** that lands in the verification log.
- **`rollback::{apply, rollback}` take a `validate::Exists` resolver** so
  evidence refs are checked across layers.
- **The `STATUS_*` string constants are gone**; `VerificationRecord.status`
  is now the typed `RecordStatus` enum.
- **`Promotion` gains a `review_of` field** (required for `kind: Review`
  promotions, which previously stranded the session by failing validation
  after the archive was written).
- **Concurrent `end_session` calls are serialized** by a lock file under
  the agent's journal; a second end now reports `already_ended` instead of
  writing a duplicate archive.
- **`add_observation`/`add_attempt` reject an already-ended session.**
  Previously the write succeeded and the next `end_session` deleted the
  notes unarchived. (MCP step-level behavior changes accordingly.)

### Fixed

- **`end_session` is idempotent.** Re-ending an archived session reports
  `already_ended` rather than writing a second journal entry; a crash
  between the journal write and scratch cleanup converges on retry.
- **Reads never create directories.** `Store::open` no longer mkdirs;
  `stats --agent nobody` no longer leaves `journal/nobody/` behind.
  `Gungnir::open` is read-only; `gungnir init` lays out the store.
- **Honest abstention.** The CLI verdict distinguishes "no knowledge on
  file", "no knowledge matched this topic", and "no live knowledge (hidden
  only)"; the coverage line is no longer suppressed when only hidden
  matches exist. Briefings say "Entries matched ... but none are verified"
  vs "No knowledge matched this task".
- **`supersede` carries evidence forward** instead of dropping the parent's
  evidence refs.
- **`promote` requires a journal source** — a codex source would cite its
  own layer, a scratch source would dangle when the session ends.
- **Typed `VerificationRecord.status`.** A typo'd log status now fails
  deserialization (and is reported by skip-and-report reads) instead of
  silently downgrading the entry to unverified under `as_of`.
- **Length caps count chars, not bytes**, matching SPEC.md's "≤ 200 chars".
- **Rollback validates evidence refs cross-layer**, like `verify` already
  did — necessary now that revisions carry evidence.

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
