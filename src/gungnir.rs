//! The Gungnir facade: high-level operations over the three layers.
//!
//! Layer mapping (see [`crate::layout`]):
//! - Codex: `<root>/codex` — shared source of truth
//! - Journal: `<root>/journal/<agent>` — private per-agent history
//! - Scratch: `<root>/scratch/<session>` — ephemeral per-task memory

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use fs2::FileExt;

use chrono::Utc;
use sha2::Digest as _;

use crate::briefing::{self, Briefing, BriefingInput};
use crate::conflicts::{self, ConflictId, ConflictRecord, ConflictStatus, ConflictVerb, Conflicts};
use crate::entry::{Entry, EntryKind, Evidence, VerificationState};
use crate::id::EntryId;
use crate::layout::{self, CODEX, CONFLICTS, JOURNAL, SCRATCH};
use crate::recall::{self, Hit, Query, SearchOutcome};
use crate::rollback;
use crate::store::Skipped;
use crate::validate::{self, MAX_SUMMARY_LEN};
use crate::{Error, Result, Store};

/// One open working session. Identity (`agent`, `task`, `started_at`) is
/// persisted to `scratch/<id>/.session.yaml` at start so a session rebuilt
/// from its id — after a CLI call boundary or an MCP restart — still knows
/// its task.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub id: String,
    pub agent: String,
    pub task: String,
    pub started_at: chrono::DateTime<Utc>,
}

/// A finding promoted from a session into the shared Codex.
#[derive(Clone, Debug)]
pub struct Promotion {
    pub kind: EntryKind,
    pub summary: String,
    pub body: String,
    /// Target when `kind` is `Review`; required by write-time validation.
    pub review_of: Option<EntryId>,
}

#[derive(Clone, Debug)]
pub struct EndReport {
    pub journal_id: EntryId,
    pub promoted: Vec<EntryId>,
    /// The session had already been archived: this end was a no-op.
    pub already_ended: bool,
    /// Conflict candidates the promoted claims raised against verified
    /// Codex facts. The journal archive itself is session bookkeeping and
    /// never raises.
    pub conflicts: Vec<ConflictRecord>,
    /// Conflict detection that degraded instead of surfacing candidates.
    /// Non-empty means a scan or queue write failed; the session still
    /// closed and every promoted entry landed.
    pub warnings: Vec<String>,
}

/// What a claim write produced: the entry id plus any conflict candidates
/// it raised against live verified facts. Candidates are advisory — the
/// write already landed; the queue just waits for a resolution verb.
#[derive(Clone, Debug)]
pub struct WriteOutcome {
    pub id: EntryId,
    pub conflicts: Vec<ConflictRecord>,
    /// Advisory-path failures that could not fail the write: a scope that
    /// failed to scan, or a candidate the queue could not persist. The
    /// caller should surface these (stderr on the CLI); the entry stands.
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Gungnir {
    root: PathBuf,
}

/// Topic-scoped counts over journal partitions other than the caller's.
/// Counts only: no agent name, summary, or body ever leaves the journal.
/// Caveat: a positive count still reveals *that* some agent wrote about
/// the topic; existence leaks, content does not. See docs/PROVENANCE.md.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct PeerCoverage {
    /// Journal partitions holding at least one topic-matched entry.
    pub agents_attempted: usize,
    /// Topic-matched journal entries across those partitions.
    pub entries: usize,
    /// Of `agents_attempted`, how many recorded a failed attempt.
    pub agents_failed: usize,
}

/// One evidence-file revalidation result. `expected_sha256` is the hash
/// recorded at citation time; `actual_sha256` is `None` when the file is
/// gone. `read_error` distinguishes a file that exists but can't be read
/// from a genuinely missing one — the CLI prints it as UNREADABLE, not
/// MISSING. Read-only: revalidation reports drift, it never rewrites
/// history.
#[derive(Clone, Debug)]
pub struct EvidenceCheck {
    pub entry: EntryId,
    pub path: String,
    pub expected_sha256: String,
    pub actual_sha256: Option<String>,
    /// Set when the file exists but reading it failed (permissions, a
    /// directory in its place, …). `actual_sha256` is `None` then too.
    pub read_error: Option<String>,
}

impl EvidenceCheck {
    /// Intact means present and hashing to the recorded value.
    pub fn intact(&self) -> bool {
        self.actual_sha256.as_deref() == Some(self.expected_sha256.as_str())
    }
}

/// What a `revalidate_all` sweep found. A corrupt entry file aborts
/// nothing: it lands in `unreadable` while the rest of the store still
/// reports, matching the per-partition tolerance peer coverage uses.
#[derive(Clone, Debug, Default)]
pub struct RevalidateReport {
    pub checks: Vec<EvidenceCheck>,
    /// Entry files that failed to parse during the sweep.
    pub unreadable: Vec<PathBuf>,
}

/// Memory health snapshot: counts by layer and verification state,
/// supersession depth, staleness. Powers `gungnir stats`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct StatsReport {
    pub codex_entries: usize,
    pub journal_entries: usize,
    pub scratch_sessions: usize,
    pub verified: usize,
    pub unverified: usize,
    pub contradicted: usize,
    pub rolled_back: usize,
    /// Entries whose id appears as the target of some other entry's
    /// `revises` link; they have newer generations.
    pub superseded: usize,
    /// Non-rolled-back entries older than 30 days.
    pub stale_over_30d: usize,
    /// verified / (verified + unverified + contradicted), 0.0 when empty.
    pub verification_rate: f64,
    /// ULID-named files that failed to parse and were skipped.
    pub skipped: Vec<Skipped>,
}

impl Gungnir {
    /// Open a facade rooted at `root`. Creates nothing: `gungnir init`
    /// lays out the layer directories and the write path creates them
    /// lazily, so read commands never touch the filesystem.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        // Canonicalize so evidence paths can be recorded relative to the
        // store (and still resolved from it later) no matter how the root
        // was spelled.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Ok(Self { root })
    }

    /// Create the layer directories for a fresh store (`gungnir init`).
    pub fn init(&self) -> Result<()> {
        for sub in [CODEX, JOURNAL, SCRATCH] {
            std::fs::create_dir_all(self.root.join(sub))?;
        }
        Ok(())
    }

    /// Resolve from env / home default.
    pub fn open_default() -> Result<Self> {
        Self::open(layout::resolve_root(None))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn codex(&self) -> Result<Store> {
        Store::open(self.root.join(CODEX))
    }

    pub fn journal(&self, agent: &str) -> Result<Store> {
        Store::open(
            self.root
                .join(JOURNAL)
                .join(layout::sanitize_component(agent)),
        )
    }

    pub fn scratch(&self, session_id: &str) -> Result<Store> {
        Store::open(
            self.root
                .join(SCRATCH)
                .join(layout::sanitize_component(session_id)),
        )
    }

    /// The conflict-candidate queue under `<root>/.conflicts/`.
    fn conflicts_queue(&self) -> Conflicts {
        Conflicts::open(self.root.join(CONFLICTS))
    }

    /// Every recorded conflict candidate, oldest first.
    pub fn all_conflicts(&self) -> Result<Vec<ConflictRecord>> {
        self.conflicts_queue().list()
    }

    /// Candidates still awaiting a resolution verb. Strict like
    /// [`Self::all_conflicts`]: a record that fails to parse is an error,
    /// not a skipped row — the audit surface is where corruption shows.
    /// Briefings read through [`Self::open_conflicts_lenient`].
    pub fn open_conflicts(&self) -> Result<Vec<ConflictRecord>> {
        self.conflicts_queue().list_open()
    }

    /// Open candidates, skipping records that fail to parse. For readers
    /// that only want a count or a warning: one corrupt file must not fail
    /// a briefing or hide the records that did parse.
    pub fn open_conflicts_lenient(&self) -> Result<Vec<ConflictRecord>> {
        self.conflicts_queue().open_lenient()
    }

    /// One recorded candidate by id, strict — corruption and missing ids
    /// both surface as errors.
    pub fn conflict_record(&self, id: ConflictId) -> Result<ConflictRecord> {
        self.conflicts_queue().require(id)
    }

    /// Candidates `agent` may see. A record's layer label says where the
    /// scan ran, not who the claim belongs to — a journal write's
    /// codex-scope candidate is labeled "codex" but describes a private
    /// claim — so visibility is derived from where both endpoints live:
    /// the shared codex or the agent's own journal. `all` includes
    /// resolved and dismissed records.
    pub fn conflicts_for(&self, agent: &str, all: bool) -> Result<Vec<ConflictRecord>> {
        let records = if all {
            self.all_conflicts()?
        } else {
            self.open_conflicts()?
        };
        let codex = self.codex()?;
        let journal = self.journal(agent)?;
        Ok(records
            .into_iter()
            .filter(|c| self.record_visible(&codex, &journal, c))
            .collect())
    }

    /// Whether `agent` may see and resolve `record`: every endpoint sits in
    /// the shared codex or the agent's own journal.
    pub fn conflict_visible_to(&self, agent: &str, record: &ConflictRecord) -> Result<bool> {
        Ok(self.record_visible(&self.codex()?, &self.journal(agent)?, record))
    }

    /// Endpoint visibility shared by `conflicts_for`, `conflict_visible_to`,
    /// and briefings: both ends exist in the shared or the agent's own
    /// store. `exists` is a path stat on already-open stores.
    fn record_visible(&self, codex: &Store, journal: &Store, c: &ConflictRecord) -> bool {
        [c.new_entry, c.conflicting_entry]
            .iter()
            .all(|id| codex.exists(*id).unwrap_or(false) || journal.exists(*id).unwrap_or(false))
    }

    /// Stores scanned for conflicts after a claim write lands in `home`:
    /// always the write's own layer, plus the shared Codex when the write
    /// lands in a Journal — journal facts are private, but disagreeing with
    /// a verified Codex claim is exactly what the loop exists to surface.
    /// One agent's journal never scans another's, and scratch is working
    /// memory, not claims, so it never scans at all. Labels come from the
    /// store path, not the writer's name: a supersede can land a revision
    /// in another agent's journal.
    fn conflict_scopes(&self, home: &Store) -> Result<Vec<(Store, String)>> {
        let codex_root = self.root.join(CODEX);
        if home.root() == codex_root {
            return Ok(vec![(home.clone(), "codex".to_string())]);
        }
        if let Ok(rel) = home.root().strip_prefix(self.root.join(JOURNAL)) {
            let mut scopes = vec![(home.clone(), format!("journal/{}", rel.display()))];
            scopes.push((Store::open(&codex_root)?, "codex".to_string()));
            return Ok(scopes);
        }
        Ok(vec![])
    }

    /// Detect + record conflict candidates for `entries` across the home
    /// layer's scopes in one pass per scope. Detection runs after the
    /// writes land and can only add records, so a failure is a warning the
    /// caller surfaces — never an error: returning Err would report a
    /// completed write as failed and tempt a duplicate retry (SPEC.md: the
    /// write still succeeds).
    fn scan_and_record(
        &self,
        home: &Store,
        entries: &[Entry],
        embedder: Option<&dyn crate::embedding::Embedder>,
    ) -> (Vec<ConflictRecord>, Vec<String>) {
        let queue = self.conflicts_queue();
        let mut records = Vec::new();
        let mut warnings = Vec::new();
        if entries.is_empty() {
            return (records, warnings);
        }
        let scopes = match self.conflict_scopes(home) {
            Ok(s) => s,
            Err(e) => return (records, vec![format!("conflict detection skipped: {e}")]),
        };
        for (store, layer) in scopes {
            let candidates = match conflicts::detect_many(&store, &layer, entries, embedder) {
                Ok(c) => c,
                Err(e) => {
                    warnings.push(format!("conflict detection on {layer} failed: {e}"));
                    continue;
                }
            };
            for rec in candidates {
                match queue.record(&rec) {
                    Ok(()) => records.push(rec),
                    Err(e) => warnings.push(format!(
                        "conflict candidate {} could not be recorded: {e}",
                        rec.id
                    )),
                }
            }
        }
        (records, warnings)
    }

    // -- writes --------------------------------------------------------------

    /// Write a claim into `layer` (Codex or the agent's Journal), then scan
    /// for conflict candidates against live verified facts. The write always
    /// lands first: conflicts are advisory records in `.conflicts/`, never a
    /// gate.
    pub fn add(&self, entry: Entry, layer: Layer<'_>) -> Result<WriteOutcome> {
        self.add_inner(entry, layer, None)
    }

    /// `add` with the semantic channel fused into conflict detection.
    pub fn add_semantic(
        &self,
        entry: Entry,
        layer: Layer<'_>,
        embedder: &dyn crate::embedding::Embedder,
    ) -> Result<WriteOutcome> {
        self.add_inner(entry, layer, Some(embedder))
    }

    fn add_inner(
        &self,
        entry: Entry,
        layer: Layer<'_>,
        embedder: Option<&dyn crate::embedding::Embedder>,
    ) -> Result<WriteOutcome> {
        let store = match layer {
            Layer::Codex => self.codex()?,
            Layer::Journal { agent } => self.journal(agent)?,
            Layer::Scratch { .. } => {
                return Err(Error::Invalid(
                    "claims land in codex or journal; scratch is session working memory".into(),
                ))
            }
        };
        store.create_with(&entry, &|id| self.exists_anywhere(id))?;
        let (conflicts, warnings) =
            self.scan_and_record(&store, std::slice::from_ref(&entry), embedder);
        Ok(WriteOutcome {
            id: entry.id,
            conflicts,
            warnings,
        })
    }

    // -- session lifecycle ---------------------------------------------------

    /// Begin a session, persisting its identity to scratch so the task text
    /// survives across process boundaries (CLI `session` calls, MCP restarts).
    pub fn start_session(
        &self,
        agent: impl Into<String>,
        task: impl Into<String>,
    ) -> Result<Session> {
        let s = Session {
            id: EntryId::generate().to_string(),
            agent: agent.into(),
            task: task.into(),
            started_at: Utc::now(),
        };
        self.persist_session(&s)?;
        Ok(s)
    }

    fn session_dir(&self, session_id: &str) -> PathBuf {
        self.root
            .join(SCRATCH)
            .join(layout::sanitize_component(session_id))
    }

    /// The journal root for `agent` plus an exclusive per-session lock
    /// file under it. end_session and add_scratch both take it, so a
    /// note's check-and-write never interleaves with an end's
    /// archive-and-delete. The file lives in the journal, which outlives
    /// scratch cleanup; Store::scan only reads ULID-named .md files, so
    /// dotfiles are invisible to it.
    fn session_lock(&self, agent: &str, session_id: &str) -> Result<(PathBuf, std::fs::File)> {
        let journal_root = self
            .root
            .join(JOURNAL)
            .join(layout::sanitize_component(agent));
        std::fs::create_dir_all(&journal_root)?;
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(journal_root.join(format!(
                ".end-{}.lock",
                layout::sanitize_component(session_id)
            )))?;
        lock_file.lock_exclusive()?;
        Ok((journal_root, lock_file))
    }

    fn session_meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir(session_id).join(".session.yaml")
    }

    /// Write the session's identity file. Skipped silently when it already
    /// exists, so a session reconstructed with an empty task never clobbers
    /// the real one. Temp-then-rename like every other write: a torn meta
    /// would fail `load_session` for the session's whole life.
    fn persist_session(&self, session: &Session) -> Result<()> {
        let path = self.session_meta_path(&session.id);
        if path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        let yaml = serde_yaml::to_string(session)
            .map_err(|e| Error::Invalid(format!("cannot serialize session {}: {e}", session.id)))?;
        let tmp = path.with_file_name(format!(".tmp-{}-session.yaml", std::process::id()));
        std::fs::write(&tmp, yaml)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Load a persisted session, if `start_session` wrote one.
    pub fn load_session(&self, session_id: &str) -> Result<Option<Session>> {
        let path = self.session_meta_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let raw = std::fs::read_to_string(&path)?;
        serde_yaml::from_str(&raw)
            .map(Some)
            .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))
    }

    /// The journal archive a session was ended into, if it was.
    fn find_archive(&self, agent: &str, session_id: &str) -> Result<Option<EntryId>> {
        let scan = self.journal(agent)?.scan()?;
        // An unreadable journal file might be the archive itself: fail
        // closed rather than read "no archive" and accept writes or a
        // second archive for an ended session.
        if let Some(bad) = scan.skipped.first() {
            return Err(Error::Invalid(format!(
                "journal for {agent} has unreadable entry {}: {}; cannot confirm whether session {session_id} is archived",
                bad.path.display(),
                bad.error
            )));
        }
        for e in scan.entries {
            if e.kind == EntryKind::Session && e.session_id.as_deref() == Some(session_id) {
                return Ok(Some(e.id));
            }
        }
        Ok(None)
    }

    pub fn add_observation(&self, session: &Session, text: impl Into<String>) -> Result<EntryId> {
        self.add_scratch(session, EntryKind::Observation, text.into(), None)
    }

    pub fn add_attempt(
        &self,
        session: &Session,
        text: impl Into<String>,
        succeeded: bool,
    ) -> Result<EntryId> {
        self.add_scratch(session, EntryKind::Attempt, text.into(), Some(succeeded))
    }

    fn add_scratch(
        &self,
        session: &Session,
        kind: EntryKind,
        text: String,
        outcome: Option<bool>,
    ) -> Result<EntryId> {
        // The session lock serializes this check-and-write against
        // end_session's archive-and-delete: an add accepted while an end
        // runs is either scanned into the archive or rejected, never
        // deleted unseen.
        let (journal_root, _end_lock) = self.session_lock(&session.agent, &session.id)?;
        let dir_exists = self.session_dir(&session.id).exists();
        // Scratch existing means the session is live: a successful end
        // deletes it, so its presence is the cheap signal that skips the
        // journal parse on every write. The `.ended-<id>` tombstone,
        // written before the archive and surviving a crashed scratch
        // cleanup, demotes a dead session's leftover scratch back to the
        // journal check — otherwise a partially ended session would keep
        // accepting notes the next end deletes unarchived.
        let tombstoned = journal_root
            .join(format!(
                ".ended-{}",
                layout::sanitize_component(&session.id)
            ))
            .exists();
        if (!dir_exists || tombstoned) && self.find_archive(&session.agent, &session.id)?.is_some()
        {
            return Err(Error::Invalid(format!(
                "session {} already ended; its notes are archived and new ones cannot be added",
                session.id
            )));
        }
        self.persist_session(session)?;
        let mut entry = Entry::new(&session.agent, kind, summarize(&text));
        entry.session_id = Some(session.id.clone());
        entry.outcome = outcome;
        entry.body = match outcome {
            Some(ok) => format!(
                "outcome: {}\n\n{}",
                if ok { "succeeded" } else { "failed" },
                text
            ),
            None => text,
        };
        self.scratch(&session.id)?.create(&entry)?;
        Ok(entry.id)
    }

    /// Archive the session into the Journal, promote findings into the Codex,
    /// then clear the scratch directory. Re-ending an already-archived
    /// session is a no-op that reports `already_ended` instead of writing a
    /// second journal entry.
    pub fn end_session(
        &self,
        session: &Session,
        summary: impl AsRef<str>,
        promotions: Vec<Promotion>,
    ) -> Result<EndReport> {
        self.end_session_inner(session, summary, promotions, None)
    }

    /// `end_session` with semantic conflict detection over the promotions.
    pub fn end_session_semantic(
        &self,
        session: &Session,
        summary: impl AsRef<str>,
        promotions: Vec<Promotion>,
        embedder: &dyn crate::embedding::Embedder,
    ) -> Result<EndReport> {
        self.end_session_inner(session, summary, promotions, Some(embedder))
    }

    fn end_session_inner(
        &self,
        session: &Session,
        summary: impl AsRef<str>,
        promotions: Vec<Promotion>,
        embedder: Option<&dyn crate::embedding::Embedder>,
    ) -> Result<EndReport> {
        let summary = summary.as_ref();
        // Serialize the check-archive-and-write-archive sequence across
        // processes and against add_scratch: without a session lock two
        // concurrent ends both see "no archive" and write duplicate
        // journal entries, and a concurrent add can be accepted into
        // scratch the delete then removes unseen.
        let (journal_root, _end_lock) = self.session_lock(&session.agent, &session.id)?;

        let dir = self.session_dir(&session.id);
        let scratch_exists = dir.exists();
        let archive = self.find_archive(&session.agent, &session.id)?;
        if archive.is_some() || !scratch_exists {
            let Some(archive) = archive else {
                return Err(Error::Invalid(format!(
                    "session {} has no scratch or journal archive",
                    session.id
                )));
            };
            // A crashed end can leave pending promotions and scratch behind;
            // finish the promotions (skipping the ones that landed) and
            // converge by clearing scratch.
            let (promoted, fresh) =
                self.finish_promotions(archive, &session.id, &session.agent, promotions)?;
            if scratch_exists {
                std::fs::remove_dir_all(&dir)?;
            }
            // Entries this retry created still get scanned; deduped ones
            // were scanned by the end that first landed them.
            let codex = self.codex()?;
            let (conflicts, warnings) = self.scan_and_record(&codex, &fresh, embedder);
            return Ok(EndReport {
                journal_id: archive,
                promoted,
                already_ended: true,
                conflicts,
                warnings,
            });
        }

        let scratch = self.scratch(&session.id)?;
        let scan = scratch.scan()?;
        if let Some(bad) = scan.skipped.first() {
            return Err(Error::Invalid(format!(
                "session {} has unreadable scratch entry {}: {}",
                session.id,
                bad.path.display(),
                bad.error
            )));
        }

        let mut transcript = String::new();
        for e in &scan.entries {
            // Prefer the structured field; fall back to the legacy
            // `outcome:` body line written by older versions.
            let outcome = e
                .outcome
                .map(|ok| format!(" [{}]", if ok { "succeeded" } else { "failed" }))
                .or_else(|| {
                    e.body
                        .strip_prefix("outcome: ")
                        .and_then(|rest| rest.lines().next())
                        .map(|w| format!(" [{w}]"))
                })
                .unwrap_or_default();
            transcript.push_str(&format!("- {}: {}{}\n", e.kind, e.summary, outcome));
        }

        // The persisted task wins over whatever a rebuilt handle claims;
        // fall back to the handle for pre-meta sessions.
        let task = self
            .load_session(&session.id)?
            .map(|s| s.task)
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| session.task.clone());

        // The tombstone goes down before the archive so an end that
        // fails between them still leaves a marker telling add_scratch
        // to consult the journal instead of trusting leftover scratch.
        std::fs::write(
            journal_root.join(format!(
                ".ended-{}",
                layout::sanitize_component(&session.id)
            )),
            b"",
        )?;

        let mut journal_entry = Entry::new(&session.agent, EntryKind::Session, summary);
        journal_entry.session_id = Some(session.id.clone());
        journal_entry.body = format!("# Task\n{task}\n\n# Transcript\n{transcript}");
        let journal_store = self.journal(&session.agent)?;
        journal_store.create(&journal_entry)?;

        // Scratch goes before promotions: once the archive exists the
        // session is ended, and a crash or validation failure in
        // finish_promotions must not leave scratch behind to keep
        // accepting notes a retry would delete unarchived. The
        // tombstone covers a crash inside remove_dir_all itself.
        let dir = self
            .root
            .join(SCRATCH)
            .join(layout::sanitize_component(&session.id));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }

        let (promoted, fresh) =
            self.finish_promotions(journal_entry.id, &session.id, &session.agent, promotions)?;

        // Scan the freshly landed promotions: entries deduped on retry
        // were already scanned by the end that created them.
        let codex = self.codex()?;
        let (conflicts, warnings) = self.scan_and_record(&codex, &fresh, embedder);

        Ok(EndReport {
            journal_id: journal_entry.id,
            promoted,
            already_ended: false,
            conflicts,
            warnings,
        })
    }

    /// Write session promotions into the Codex, each citing the archive as
    /// evidence. Retries dedupe on `session_id` + `summary`, so a crash or
    /// validation failure mid-loop leaves the next end_session able to
    /// finish the set without duplicating what landed.
    /// Returns every promoted id plus the entries this call created —
    /// deduped retries report the id but skip the conflict scan the
    /// first end already ran on the landed entry.
    fn finish_promotions(
        &self,
        archive: EntryId,
        session_id: &str,
        agent: &str,
        promotions: Vec<Promotion>,
    ) -> Result<(Vec<EntryId>, Vec<Entry>)> {
        let codex = self.codex()?;
        // Dedupe on (kind, summary, body, review_of): two different
        // findings can share a summary, and a retry re-listing a promotion
        // must land it rather than alias the landed one — but only when
        // the body also matches, so distinct findings keep their own text.
        // review_of belongs in the key: two reviews with identical prose
        // of different targets are different promotions.
        let mut by_key: HashMap<(EntryKind, String, String, Option<EntryId>), EntryId> = codex
            .entries()?
            .into_iter()
            .filter(|e| e.session_id.as_deref() == Some(session_id))
            .map(|e| ((e.kind, e.summary, e.body, e.review_of), e.id))
            .collect();
        let mut promoted_ids = Vec::new();
        let mut fresh = Vec::new();
        for p in promotions {
            let key = (p.kind, p.summary.clone(), p.body.clone(), p.review_of);
            if let Some(&existing) = by_key.get(&key) {
                promoted_ids.push(existing);
                continue;
            }
            let mut c = Entry::new(agent, p.kind, p.summary);
            c.session_id = Some(session_id.to_string());
            c.body = p.body;
            c.review_of = p.review_of;
            c.evidence.push(Evidence::Ref { id: archive });
            codex.create_with(&c, &|id| self.exists_anywhere(id))?;
            by_key.insert(key, c.id);
            promoted_ids.push(c.id);
            fresh.push(c);
        }
        Ok((promoted_ids, fresh))
    }

    // -- reading -------------------------------------------------------------

    /// Compile the pre-task briefing: Codex facts plus this agent's prior
    /// attempts relevant to `task`, with coverage driving the abstention
    /// signal.
    pub fn brief(&self, agent: &str, task: &str, limit: usize) -> Result<Briefing> {
        self.brief_inner(agent, task, limit, None)
    }

    /// Briefing through the hybrid vector + keyword path. The abstention
    /// signal is the same one the keyword briefing uses; it only gets more
    /// honest, since paraphrased knowledge now counts toward coverage.
    pub fn brief_semantic(
        &self,
        agent: &str,
        task: &str,
        limit: usize,
        embedder: &dyn crate::embedding::Embedder,
    ) -> Result<Briefing> {
        self.brief_inner(agent, task, limit, Some(embedder))
    }

    fn brief_inner(
        &self,
        agent: &str,
        task: &str,
        limit: usize,
        embedder: Option<&dyn crate::embedding::Embedder>,
    ) -> Result<Briefing> {
        let codex = self.codex()?;
        let journal = self.journal(agent)?;
        let q = Query::new(task, limit);

        let search = |store: &Store, q: &Query| -> Result<SearchOutcome> {
            match embedder {
                Some(e) => crate::embedding::hybrid_search(store, q, e),
                None => recall::search_with_coverage(store, q),
            }
        };
        let codex_out = search(&codex, &q)?;
        let journal_out = search(&journal, &q)?;
        // Open conflict candidates touching this topic's hits warn in the
        // briefing — the same role the CONTRADICTED warning plays, one step
        // earlier in the lifecycle.
        let hit_ids: HashSet<EntryId> = codex_out
            .hits
            .iter()
            .chain(&journal_out.hits)
            .map(|h| h.entry.id)
            .collect();
        // A candidate is "on this topic" when an endpoint was a hit, or its
        // snapshot shares a task token — hits truncate to `limit`, so
        // hit-only matching would hide conflicts between facts that ranked
        // just below the briefing cutoff.
        let task_tokens: HashSet<String> = recall::tokenize(task).into_iter().collect();
        let on_topic = |c: &ConflictRecord| {
            hit_ids.contains(&c.new_entry)
                || hit_ids.contains(&c.conflicting_entry)
                || recall::tokenize(&c.new_summary)
                    .iter()
                    .any(|t| task_tokens.contains(t))
                || recall::tokenize(&c.conflicting_summary)
                    .iter()
                    .any(|t| task_tokens.contains(t))
        };
        // ...and only warn on candidates the agent may see. Briefings are
        // agent-scoped (codex + own journal), and a record's layer label is
        // where the scan ran, not who the claim belongs to: a journal
        // write's codex-scope candidate is labeled "codex" but its new
        // claim is private. Existence checks run against the already-open
        // stores — locate() would reopen and resweep a store per endpoint.
        let mut open_conflicts = 0;
        for c in self.open_conflicts_lenient()? {
            if !on_topic(&c) {
                continue;
            }
            if self.record_visible(&codex, &journal, &c) {
                open_conflicts += 1;
            }
        }
        Ok(briefing::compile(BriefingInput {
            task: task.to_string(),
            codex_hits: codex_out.hits,
            journal_hits: journal_out.hits,
            codex_superseded: superseded_ids(&codex)?,
            journal_superseded: superseded_ids(&journal)?,
            codex_coverage: codex_out.coverage,
            journal_coverage: journal_out.coverage,
            codex_skipped: codex_out.skipped,
            journal_skipped: journal_out.skipped,
            peers: self.peer_coverage_inner(Some(agent), task, embedder)?,
            open_conflicts,
        }))
    }

    /// Cross-agent coverage for one topic: aggregate counts over every
    /// journal partition except `agent`'s. Stops a second agent repeating a
    /// documented failure without ever reading the failure itself; entries
    /// are counted, never surfaced.
    ///
    /// `agent: None` counts every journal partition.
    pub fn peer_coverage(&self, agent: Option<&str>, topic: &str) -> Result<PeerCoverage> {
        self.peer_coverage_inner(agent, topic, None)
    }

    /// [`Self::peer_coverage`] through the hybrid vector + keyword path, so
    /// a semantic briefing sees peers whose journals paraphrase the topic.
    /// Failure attribution stays keyword-based: a session's failed lines
    /// must share stemmed tokens with the topic to count.
    pub fn peer_coverage_semantic(
        &self,
        agent: Option<&str>,
        topic: &str,
        embedder: &dyn crate::embedding::Embedder,
    ) -> Result<PeerCoverage> {
        self.peer_coverage_inner(agent, topic, Some(embedder))
    }

    fn peer_coverage_inner(
        &self,
        agent: Option<&str>,
        topic: &str,
        embedder: Option<&dyn crate::embedding::Embedder>,
    ) -> Result<PeerCoverage> {
        let base = self.root.join(JOURNAL);
        let exclude = agent.map(layout::sanitize_component);
        let q = Query::new(topic, usize::MAX);
        let mut pc = PeerCoverage::default();
        for name in layer_child_names(&base)? {
            if Some(&name) == exclude.as_ref() {
                continue;
            }
            // Best-effort: a corrupt file in a peer's journal must not fail
            // this agent's briefing. Skipped partitions simply don't count.
            let Ok(out) = Store::open(base.join(&name)).and_then(|s| match embedder {
                Some(e) => crate::embedding::hybrid_search(&s, &q, e),
                None => recall::search_with_coverage(&s, &q),
            }) else {
                continue;
            };
            if out.hits.is_empty() {
                continue;
            }
            pc.agents_attempted += 1;
            pc.entries += out.hits.len();
            if out.hits.iter().any(|h| failed_attempt(h, topic)) {
                pc.agents_failed += 1;
            }
        }
        Ok(pc)
    }

    /// Re-hash every file-evidence link on `id` and compare against the
    /// SHA-256 recorded at citation time. A missing file reports
    /// `actual_sha256: None`. Entry refs are skipped: they resolve to live
    /// entries whose history the verification log already tracks.
    pub fn revalidate(&self, id: EntryId) -> Result<Vec<EvidenceCheck>> {
        let store = self.locate(id)?.ok_or(Error::NotFound(id))?;
        let entry = store.require(id)?;
        Ok(check_evidence(&self.root, &entry))
    }

    /// [`Self::revalidate`] over every entry in every layer. Entry files
    /// that fail to parse are collected in `unreadable` rather than
    /// aborting the sweep: one corrupt file must not hide the drift report
    /// for everything else.
    pub fn revalidate_all(&self) -> Result<RevalidateReport> {
        let mut report = RevalidateReport::default();
        sweep_store(&self.codex()?, &self.root, &mut report);
        for base_name in [JOURNAL, SCRATCH] {
            let base = self.root.join(base_name);
            for name in layer_child_names(&base)? {
                sweep_store(&Store::open(base.join(&name))?, &self.root, &mut report);
            }
        }
        Ok(report)
    }

    /// Search one layer with coverage accounting.
    pub fn search_layer(&self, layer: Layer, query: &Query) -> Result<SearchOutcome> {
        match layer {
            Layer::Codex => recall::search_with_coverage(&self.codex()?, query),
            Layer::Journal { agent } => recall::search_with_coverage(&self.journal(agent)?, query),
            Layer::Scratch { session_id } => {
                recall::search_with_coverage(&self.scratch(session_id)?, query)
            }
        }
    }

    /// Hybrid vector + keyword search over one layer. Same candidate pool,
    /// same coverage accounting as [`Self::search_layer`]; the vector channel
    /// only widens what "topic-matched" means.
    pub fn search_layer_semantic(
        &self,
        layer: Layer<'_>,
        query: &Query,
        embedder: &dyn crate::embedding::Embedder,
    ) -> Result<SearchOutcome> {
        match layer {
            Layer::Codex => crate::embedding::hybrid_search(&self.codex()?, query, embedder),
            Layer::Journal { agent } => {
                crate::embedding::hybrid_search(&self.journal(agent)?, query, embedder)
            }
            Layer::Scratch { session_id } => {
                crate::embedding::hybrid_search(&self.scratch(session_id)?, query, embedder)
            }
        }
    }

    /// Keyword recall restricted to one layer.
    pub fn recall_layer(&self, layer: Layer, query: &Query) -> Result<Vec<Hit>> {
        self.search_layer(layer, query).map(|o| o.hits)
    }

    // -- mutations ------------------------------------------------------------

    /// Reference resolution across every layer. Evidence may point anywhere
    /// (Codex fact citing its Journal archive, etc.), so all mutation paths
    /// validate against the union.
    fn exists_anywhere(&self, id: EntryId) -> Result<bool> {
        Ok(self.locate(id)?.is_some())
    }

    pub fn verify(&self, id: EntryId, verifier: &str, note: Option<String>) -> Result<()> {
        let store = self.locate(id)?.ok_or(Error::NotFound(id))?;
        let mut entry = store.require(id)?;
        entry.verify(verifier, note);
        store.update_with(&entry, &|id| self.exists_anywhere(id))
    }

    /// Verify and record file evidence on the entry in one update: the
    /// SHA-256 of each path's current content, plus an excerpt from its
    /// start. Later `revalidate` calls can then prove the cited file still
    /// says what it said.
    pub fn verify_with_evidence(
        &self,
        id: EntryId,
        verifier: &str,
        note: Option<String>,
        evidence_paths: &[PathBuf],
    ) -> Result<()> {
        let store = self.locate(id)?.ok_or(Error::NotFound(id))?;
        let mut entry = store.require(id)?;
        for path in evidence_paths {
            let raw = std::fs::read(path)?;
            let digest = format!("{:x}", sha2::Sha256::digest(&raw));
            let text = String::from_utf8_lossy(&raw);
            let text = text.trim_start();
            let mut end = validate::MAX_EXCERPT_LEN.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            // Canonicalize first so a relative citation still resolves; then
            // prefer a root-relative path so a committed codex stays
            // portable across machines and clones. Files outside the store
            // keep their absolute path.
            let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            let recorded = canonical
                .strip_prefix(&self.root)
                .map(|p| p.to_path_buf())
                .unwrap_or(canonical);
            entry.evidence.push(Evidence::File {
                path: recorded.display().to_string(),
                excerpt: text[..end].to_string(),
                sha256: digest,
            });
        }
        entry.verify(verifier, note);
        store.update_with(&entry, &|id| self.exists_anywhere(id))
    }

    /// Mark `id` contradicted by `by` — the same transition the conflict
    /// resolution's Contradict verb runs, with the same refutation policy:
    /// refuses rolled-back targets and refuses to replace a
    /// `contradicted_by` naming a different entry, so callers can't swap an
    /// earlier refutation by going through the direct verb.
    pub fn contradict(
        &self,
        id: EntryId,
        by: EntryId,
        verifier: &str,
        note: Option<String>,
    ) -> Result<()> {
        let store = self.locate(id)?.ok_or(Error::NotFound(id))?;
        if !self.exists_anywhere(by)? {
            return Err(Error::NotFound(by));
        }
        let mut entry = store.require(id)?;
        if apply_contradiction(&mut entry, by, verifier, note)? {
            store.update_with(&entry, &|id| self.exists_anywhere(id))?;
        }
        Ok(())
    }

    /// Close a conflict candidate with one of the three resolution verbs.
    ///
    /// - `Supersede` ("I'm newer"): link `new_entry.revises` to the
    ///   conflicting entry. The verified fact stays `verified` but is now
    ///   superseded, so `current` views hide it — the same end state the
    ///   write-time `supersede` verb produces. Refuses to relink when the
    ///   new entry already revises something else, and refuses across
    ///   layers: supersession only hides the parent under the store that
    ///   holds the revision, so a journal revision cannot supersede a codex
    ///   fact (contradict or dismiss instead).
    /// - `Contradict` ("it's wrong"): mark the verified entry contradicted
    ///   by the new one, through the same logged transition `contradict`
    ///   uses everywhere.
    /// - `Dismiss` ("not a conflict"): close the record; entries untouched.
    ///
    /// The whole resolution — re-read, the Open check, the entry mutation,
    /// and the record close — runs inside the queue's flock via
    /// `Conflicts::resolve_locked`, so two resolvers can't both observe Open
    /// and race different verbs onto the same candidate. Entry mutations
    /// still run before the record closes: a crash leaves the candidate
    /// open rather than resolved-but-unapplied. Lock order is queue →
    /// entry-store, the only composite acquisition in the codebase, so no
    /// cycle exists (writers take a store lock then release it before ever
    /// touching the queue).
    pub fn resolve_conflict(
        &self,
        id: ConflictId,
        verb: ConflictVerb,
        actor: &str,
    ) -> Result<ConflictRecord> {
        let queue = self.conflicts_queue();
        queue.resolve_locked(id, |rec| {
            let mut rec = rec.clone();
            match verb {
                ConflictVerb::Supersede => {
                    let store = self
                        .locate(rec.new_entry)?
                        .ok_or(Error::NotFound(rec.new_entry))?;
                    let old_store = self
                        .locate(rec.conflicting_entry)?
                        .ok_or(Error::NotFound(rec.conflicting_entry))?;
                    if old_store.root() != store.root() {
                        return Err(Error::Invalid(
                            "supersede cannot span layers: the revision would not hide the superseded fact (use --contradict or --dismiss)".into(),
                        ));
                    }
                    let mut entry = store.require(rec.new_entry)?;
                    match entry.revises {
                        None => {
                            entry.revises = Some(rec.conflicting_entry);
                            store.update_with(&entry, &|id| self.exists_anywhere(id))?;
                        }
                        Some(prev) if prev == rec.conflicting_entry => {}
                        Some(prev) => {
                            return Err(Error::Invalid(format!(
                                "{} already revises {prev}; refusing to relink",
                                rec.new_entry
                            )));
                        }
                    }
                }
                ConflictVerb::Contradict => {
                    if !self.exists_anywhere(rec.new_entry)? {
                        return Err(Error::NotFound(rec.new_entry));
                    }
                    let store = self
                        .locate(rec.conflicting_entry)?
                        .ok_or(Error::NotFound(rec.conflicting_entry))?;
                    let mut entry = store.require(rec.conflicting_entry)?;
                    if apply_contradiction(&mut entry, rec.new_entry, actor, None)? {
                        store.update_with(&entry, &|id| self.exists_anywhere(id))?;
                    }
                }
                ConflictVerb::Dismiss => {}
            }

            rec.status = match verb {
                ConflictVerb::Supersede => ConflictStatus::ResolvedSuperseded,
                ConflictVerb::Contradict => ConflictStatus::ResolvedContradicted,
                ConflictVerb::Dismiss => ConflictStatus::Dismissed,
            };
            rec.resolved_by = Some(actor.to_string());
            rec.resolved_at = Some(Utc::now());
            Ok(rec)
        })
    }

    /// Write a revision of `old`: same kind and project, linked by `revises`.
    /// The new claim is still scanned for conflicts — revising one fact does
    /// not clear it of disagreeing with a *different* verified fact.
    pub fn supersede(
        &self,
        old: EntryId,
        agent: &str,
        summary: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<WriteOutcome> {
        self.supersede_inner(old, agent, summary, body, None)
    }

    /// `supersede` with the semantic channel fused into conflict detection.
    pub fn supersede_semantic(
        &self,
        old: EntryId,
        agent: &str,
        summary: impl Into<String>,
        body: impl Into<String>,
        embedder: &dyn crate::embedding::Embedder,
    ) -> Result<WriteOutcome> {
        self.supersede_inner(old, agent, summary, body, Some(embedder))
    }

    fn supersede_inner(
        &self,
        old: EntryId,
        agent: &str,
        summary: impl Into<String>,
        body: impl Into<String>,
        embedder: Option<&dyn crate::embedding::Embedder>,
    ) -> Result<WriteOutcome> {
        let store = self.locate(old)?.ok_or(Error::NotFound(old))?;
        let prev = store.require(old)?;
        let mut next = Entry::new(agent, prev.kind, summary);
        next.revises = Some(old);
        next.project_id = prev.project_id.clone();
        next.session_id = prev.session_id.clone();
        next.review_of = prev.review_of;
        // Carry evidence forward — but only refs that still resolve. Stores
        // written before promote was journal-only can hold refs to deleted
        // scratch entries; refusing to revise those entries would brick the
        // correction workflow on them.
        let mut evidence = Vec::new();
        for ev in prev.evidence {
            let keep = match &ev {
                Evidence::Ref { id } => self.exists_anywhere(*id)?,
                Evidence::File { .. } => true,
            };
            if keep {
                evidence.push(ev);
            }
        }
        next.evidence = evidence;
        next.body = body.into();
        store.create_with(&next, &|id| self.exists_anywhere(id))?;
        let (conflicts, warnings) =
            self.scan_and_record(&store, std::slice::from_ref(&next), embedder);
        Ok(WriteOutcome {
            id: next.id,
            conflicts,
            warnings,
        })
    }

    pub fn rollback(&self, target: EntryId, agent: &str) -> Result<EntryId> {
        let store = self.locate(target)?.ok_or(Error::NotFound(target))?;
        rollback::rollback(&store, target, agent, &|id| self.exists_anywhere(id))
    }

    /// Memory health across all layers. `agent` scopes the journal count;
    /// scratch counts sessions, not entries.
    pub fn stats(&self, agent: Option<&str>) -> Result<StatsReport> {
        let mut r = StatsReport {
            codex_entries: 0,
            journal_entries: 0,
            scratch_sessions: 0,
            verified: 0,
            unverified: 0,
            contradicted: 0,
            rolled_back: 0,
            superseded: 0,
            stale_over_30d: 0,
            verification_rate: 0.0,
            skipped: vec![],
        };

        let mut all: Vec<Entry> = Vec::new();
        let codex_scan = self.codex()?.scan()?;
        r.codex_entries = codex_scan.entries.len();
        r.superseded = codex_scan
            .entries
            .iter()
            .filter(supersedes)
            .filter_map(|e| e.revises)
            .collect::<HashSet<_>>()
            .len();
        all.extend(codex_scan.entries);
        r.skipped.extend(codex_scan.skipped);

        let journal_base = self.root.join(JOURNAL);
        let agents: Vec<_> = match agent {
            Some(a) => vec![layout::sanitize_component(a)],
            None => layer_child_names(&journal_base)?,
        };
        for a in agents {
            let scan = Store::open(journal_base.join(a))?.scan()?;
            r.journal_entries += scan.entries.len();
            all.extend(scan.entries);
            r.skipped.extend(scan.skipped);
        }

        let scratch_base = self.root.join(SCRATCH);
        for sess in layer_child_names(&scratch_base)? {
            let scan = Store::open(scratch_base.join(&sess))?.scan()?;
            // A started-but-empty session still counts: `.session.yaml`
            // is written before the first note, so entries alone
            // under-report live sessions.
            let sess_dir = scratch_base.join(&sess);
            if !scan.entries.is_empty() || sess_dir.join(".session.yaml").exists() {
                r.scratch_sessions += 1;
            }
            all.extend(scan.entries);
            r.skipped.extend(scan.skipped);
        }

        use chrono::Utc;
        let now = Utc::now();
        for e in &all {
            match &e.verification {
                VerificationState::Verified => r.verified += 1,
                VerificationState::Unverified => r.unverified += 1,
                VerificationState::Contradicted { .. } => r.contradicted += 1,
                VerificationState::RolledBack => r.rolled_back += 1,
            }
        }
        r.stale_over_30d = all
            .iter()
            .filter(|e| {
                e.verification != VerificationState::RolledBack
                    && (now - e.timestamp).num_days() > 30
            })
            .count();

        let decidable = r.verified + r.unverified + r.contradicted;
        if decidable > 0 {
            r.verification_rate = r.verified as f64 / decidable as f64;
        }
        Ok(r)
    }

    /// Promote a Journal entry's finding into the Codex with a provenance
    /// link back to `from`. Sources must live in the Journal: a Codex
    /// source would cite its own layer, and a Scratch source would cite a
    /// file that `end_session` deletes. The promoted claim is scanned for
    /// conflicts against live verified Codex facts.
    pub fn promote(
        &self,
        from: EntryId,
        agent: &str,
        kind: EntryKind,
        summary: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<WriteOutcome> {
        self.promote_inner(from, agent, kind, summary, body, None)
    }

    /// `promote` with the semantic channel fused into conflict detection.
    pub fn promote_semantic(
        &self,
        from: EntryId,
        agent: &str,
        kind: EntryKind,
        summary: impl Into<String>,
        body: impl Into<String>,
        embedder: &dyn crate::embedding::Embedder,
    ) -> Result<WriteOutcome> {
        self.promote_inner(from, agent, kind, summary, body, Some(embedder))
    }

    fn promote_inner(
        &self,
        from: EntryId,
        agent: &str,
        kind: EntryKind,
        summary: impl Into<String>,
        body: impl Into<String>,
        embedder: Option<&dyn crate::embedding::Embedder>,
    ) -> Result<WriteOutcome> {
        let store = self.locate(from)?.ok_or(Error::NotFound(from))?;
        if !store.root().starts_with(self.root.join(JOURNAL)) {
            return Err(Error::Invalid(format!(
                "promote sources must be journal entries; {from} is not under journal/"
            )));
        }
        let codex = self.codex()?;
        let mut c = Entry::new(agent, kind, summary);
        c.body = body.into();
        c.evidence.push(Evidence::Ref { id: from });
        if kind == EntryKind::Review {
            c.review_of = Some(from);
        }
        codex.create_with(&c, &|id| self.exists_anywhere(id))?;
        let (conflicts, warnings) =
            self.scan_and_record(&codex, std::slice::from_ref(&c), embedder);
        Ok(WriteOutcome {
            id: c.id,
            conflicts,
            warnings,
        })
    }

    /// Find which layer partition holds `id`. Cheap because paths are
    /// date-sharded from the id itself; no full scans of unrelated layers.
    pub fn locate(&self, id: EntryId) -> Result<Option<Store>> {
        let codex = self.codex()?;
        if codex.exists(id)? {
            return Ok(Some(codex));
        }
        for base_name in [JOURNAL, SCRATCH] {
            let base = self.root.join(base_name);
            if let Some(dir) = find_under(&base, id)? {
                return Ok(Some(Store::open(dir)?));
            }
        }
        Ok(None)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Layer<'a> {
    Codex,
    Journal { agent: &'a str },
    Scratch { session_id: &'a str },
}

fn find_under(base: &Path, id: EntryId) -> Result<Option<PathBuf>> {
    for name in layer_child_names(base)? {
        let dir = base.join(&name);
        if Store::open(&dir)?.exists(id)? {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}

/// Directories under a layer root that look like Gungnir created them.
/// Skips files, names that would not survive sanitization, and child
/// symlinks so `stats`/`locate` do not walk out of the store.
fn layer_child_names(base: &Path) -> Result<Vec<String>> {
    if !base.exists() {
        return Ok(vec![]);
    }
    let mut names = Vec::new();
    for dent in std::fs::read_dir(base)? {
        let dent = dent?;
        let name = dent.file_name().to_string_lossy().into_owned();
        if layout::sanitize_component(&name) != name {
            continue;
        }
        let meta = match std::fs::symlink_metadata(dent.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            continue;
        }
        names.push(name);
    }
    Ok(names)
}

/// Whether `e` is a live revision whose `revises` link supersedes its
/// target. A rollback record's `revises` points at the ancestor it
/// restores — it revives that fact, not replaces it — and a rolled-back
/// revision no longer stands, so neither hides its predecessor.
fn supersedes(e: &&Entry) -> bool {
    e.kind != EntryKind::Rollback && e.verification != VerificationState::RolledBack
}

/// Whether an entry records a failed attempt *on this topic*. Attempt
/// bodies open with `outcome: failed` (written by
/// [`Gungnir::add_attempt`]); a session archive carries the same fact on
/// transcript lines that read `- attempt: <summary> [failed]`. A raw
/// substring match would count task text or a summary that merely mentions
/// "[failed]", and counting every failed line in a topic-matched session
/// would blame the topic for an unrelated failure — the failed line must
/// share stemmed tokens with `topic` to count.
fn failed_attempt(h: &crate::recall::Hit, topic: &str) -> bool {
    let e = &h.entry;
    if e.kind == EntryKind::Attempt {
        // The entry matched the topic search, so the attempt itself is
        // on-topic by construction.
        return e.body.starts_with("outcome: failed");
    }
    if e.kind != EntryKind::Session {
        return false;
    }
    let topic_tokens = recall::tokenize(topic);
    e.body.lines().any(|l| {
        let l = l.trim_start();
        // Tokenize only the attempt summary: the archive's own wrapper
        // tokens ("attempt", "failed") are formatting, not topic evidence,
        // and would make every failed line match a topic about failures.
        let summary = l
            .strip_prefix("- attempt:")
            .and_then(|s| s.strip_suffix("[failed]"));
        match summary {
            None => false,
            Some(s) => recall::tokenize(s).iter().any(|t| topic_tokens.contains(t)),
        }
    })
}

/// Check one store's entries into `report`. Entry files that fail to parse
/// land in `unreadable` and the sweep continues: one corrupt file is a row
/// to report, not a reason to hide the rest of the store.
fn sweep_store(store: &Store, root: &Path, report: &mut RevalidateReport) {
    match store.scan() {
        Ok(scan) => {
            for entry in &scan.entries {
                report.checks.extend(check_evidence(root, entry));
            }
            report
                .unreadable
                .extend(scan.skipped.into_iter().map(|s| s.path));
        }
        Err(_) => report.unreadable.push(store.root().to_path_buf()),
    }
}

/// Revalidate one entry's file-evidence links; entry refs skip straight
/// through. A missing file reports `None` rather than erroring; drift is
/// data, not a failure of the revalidation itself. Paths stored relative
/// to the store root resolve against `root`; absolute ones are used as-is.
fn check_evidence(root: &Path, entry: &Entry) -> Vec<EvidenceCheck> {
    entry
        .evidence
        .iter()
        .filter_map(|ev| match ev {
            Evidence::File { path, sha256, .. } => {
                let file = Path::new(path);
                let file = if file.is_absolute() {
                    file.to_path_buf()
                } else {
                    root.join(file)
                };
                let (actual, read_error) = match std::fs::read(&file) {
                    Ok(raw) => (Some(format!("{:x}", sha2::Sha256::digest(&raw))), None),
                    // Missing and unreadable are different findings: one is
                    // a moved file, the other a permissions or I/O problem.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, None),
                    Err(e) => (None, Some(e.to_string())),
                };
                Some(EvidenceCheck {
                    entry: entry.id,
                    path: path.clone(),
                    expected_sha256: sha256.clone(),
                    actual_sha256: actual,
                    read_error,
                })
            }
            Evidence::Ref { .. } => None,
        })
        .collect()
}

/// Ids that some newer live entry revises, i.e. facts with a newer
/// version still in force.
pub fn superseded_ids(store: &Store) -> Result<HashSet<EntryId>> {
    Ok(store
        .entries()?
        .iter()
        .filter(supersedes)
        .filter_map(|e| e.revises)
        .collect())
}

fn summarize(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    let mut s: String = first_line.chars().take(MAX_SUMMARY_LEN).collect();
    if s.is_empty() {
        s = "(untitled)".into();
    }
    s
}

/// The refutation policy shared by `Gungnir::contradict` and the resolve
/// path's Contradict verb: refuse rolled-back targets, and refuse to
/// replace a `contradicted_by` naming a different entry so a direct verb
/// call can't swap an earlier refutation. Returns true when the
/// transition was applied; already-contradicted-by-`by` is an idempotent
/// no-op.
fn apply_contradiction(
    entry: &mut Entry,
    by: EntryId,
    verifier: &str,
    note: Option<String>,
) -> Result<bool> {
    if entry.verification == VerificationState::RolledBack {
        return Err(Error::Invalid(format!(
            "{} is rolled back; contradict applies to live facts",
            entry.id
        )));
    }
    match &entry.verification {
        VerificationState::Contradicted { by: existing } if *existing == by => Ok(false),
        VerificationState::Contradicted { by: existing } => Err(Error::Invalid(format!(
            "{} is already contradicted by {existing}; refusing to replace that refutation",
            entry.id
        ))),
        _ => {
            entry.contradict(by, verifier, note);
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gng() -> (tempfile::TempDir, Gungnir) {
        let dir = tempfile::tempdir().unwrap();
        let g = Gungnir::open(dir.path()).unwrap();
        (dir, g)
    }

    #[test]
    fn full_session_flow_archives_promotes_and_clears_scratch() {
        let (_d, g) = gng();
        let s = g
            .start_session("builder", "fix slow checkout query")
            .unwrap();
        g.add_observation(&s, "EXPLAIN shows seq scan on orders_archive")
            .unwrap();
        g.add_attempt(&s, "added index on orders.user_id", false)
            .unwrap();
        g.add_attempt(&s, "rewrote query to use orders_archive_idx", true)
            .unwrap();

        let report = g
            .end_session(
                &s,
                "rewrote checkout query to use orders_archive_idx",
                vec![Promotion {
                    kind: EntryKind::Decision,
                    summary: "checkout queries must use orders_archive_idx".into(),
                    body: "seq scan on orders_archive was the bottleneck".into(),
                    review_of: None,
                }],
            )
            .unwrap();

        let journal = g.journal("builder").unwrap();
        let archived = journal.require(report.journal_id).unwrap();
        assert_eq!(archived.kind, EntryKind::Session);
        assert!(archived.body.contains("seq scan"));
        assert!(archived.body.contains("[failed]"));

        let codex = g.codex().unwrap();
        let promoted = codex.require(report.promoted[0]).unwrap();
        assert_eq!(
            promoted.evidence,
            vec![Evidence::Ref {
                id: report.journal_id
            }]
        );

        // Scratch cleared.
        assert!(
            g.scratch(&s.id).unwrap().entries().unwrap().is_empty()
                || !g
                    .root
                    .join(SCRATCH)
                    .join(layout::sanitize_component(&s.id))
                    .exists()
        );
    }

    #[test]
    fn briefing_surfaces_codex_and_own_journal_only() {
        let (_d, g) = gng();

        // Another agent's failure must NOT appear in builder's briefing.
        let other = g.start_session("scout", "checkout perf").unwrap();
        g.add_attempt(&other, "tried vacuum full on orders", false)
            .unwrap();
        g.end_session(&other, "vacuum did not help", vec![])
            .unwrap();

        let mine = g.start_session("builder", "fix slow checkout").unwrap();
        g.add_attempt(&mine, "added checkout cache", true).unwrap();
        g.end_session(&mine, "cache helped", vec![]).unwrap();

        let b = g.brief("builder", "slow checkout", 10).unwrap();
        assert!(b.markdown.contains("checkout cache"), "{}", b.markdown);
        assert!(!b.markdown.contains("vacuum"), "{}", b.markdown);
    }

    /// A paraphrase the keyword brief misses must still reach the codex
    /// through the semantic path — this is the regression brief_semantic
    /// exists for.
    struct Dict;
    impl crate::embedding::Embedder for Dict {
        fn model(&self) -> &str {
            "dict-1"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    let l = t.to_lowercase();
                    if l.contains("postgres") || l.contains("connection") {
                        vec![1.0f32, 0.0]
                    } else {
                        vec![0.0f32, 1.0]
                    }
                })
                .collect())
        }
    }

    #[test]
    fn brief_semantic_surfaces_paraphrased_codex_knowledge() {
        let (_d, g) = gng();
        let codex = g.codex().unwrap();
        let mut e = Entry::new("ops", EntryKind::Decision, "postgres pooling limits");
        e.verify("ops", None);
        codex.create(&e).unwrap();

        let kw = g.brief("builder", "database connection tuning", 8).unwrap();
        assert!(
            !kw.markdown.contains("postgres pooling limits"),
            "premise: keyword brief misses the paraphrase\n{}",
            kw.markdown
        );
        let sem = g
            .brief_semantic("builder", "database connection tuning", 8, &Dict)
            .unwrap();
        assert!(
            sem.markdown.contains("postgres pooling limits"),
            "{}",
            sem.markdown
        );
        assert!(
            !sem.markdown
                .contains("No verified knowledge covers this task"),
            "{}",
            sem.markdown
        );
    }

    #[test]
    fn supersede_links_and_locate_finds_layers() {
        let (_d, g) = gng();
        let codex = g.codex().unwrap();
        let v1 = Entry::new("a", EntryKind::Decision, "use mysql");
        codex.create(&v1).unwrap();

        let v2 = g
            .supersede(v1.id, "a", "use postgres", "migration done")
            .unwrap()
            .id;
        let stored = codex.require(v2).unwrap();
        assert_eq!(stored.revises, Some(v1.id));

        let found = g.locate(v1.id).unwrap().unwrap();
        assert_eq!(found.root(), g.root.join(CODEX));
    }

    #[test]
    fn verify_via_facade_persists() {
        let (_d, g) = gng();
        let codex = g.codex().unwrap();
        let e = Entry::new("a", EntryKind::Observation, "load balancer healthy");
        codex.create(&e).unwrap();

        g.verify(e.id, "ops", Some("checked dashboard".into()))
            .unwrap();
        let after = g.locate(e.id).unwrap().unwrap().require(e.id).unwrap();
        assert_eq!(
            after.verification,
            crate::entry::VerificationState::Verified
        );
        assert_eq!(after.verification_log.len(), 1);
    }

    #[test]
    fn verify_works_on_promoted_entries_with_cross_layer_evidence() {
        // Regression: update-path validation used to resolve evidence refs
        // against one store only, so verifying a Codex entry whose evidence
        // cites its Journal archive failed.
        let (_d, g) = gng();
        let s = g.start_session("builder", "tune cache").unwrap();
        g.add_observation(&s, "cache hit rate 12%").unwrap();
        let report = g.end_session(&s, "raised hit rate", vec![]).unwrap();

        let cid = g
            .promote(
                report.journal_id,
                "builder",
                EntryKind::Decision,
                "cache tuning worked",
                "hit rate now 40%",
            )
            .unwrap()
            .id;
        g.verify(cid, "team", None).unwrap();

        let stored = g.codex().unwrap().require(cid).unwrap();
        assert_eq!(
            stored.verification,
            crate::entry::VerificationState::Verified
        );
    }

    #[test]
    fn stats_reports_layer_counts_and_health() {
        let (_d, g) = gng();
        let s = g.start_session("builder", "tune checkout").unwrap();
        g.add_observation(&s, "hit rate low").unwrap();
        let report = g
            .end_session(
                &s,
                "cache tuned",
                vec![Promotion {
                    kind: EntryKind::Decision,
                    summary: "checkout cache sized at 512mb".into(),
                    body: "measured hit rate gain".into(),
                    review_of: None,
                }],
            )
            .unwrap();
        g.verify(report.promoted[0], "human", None).unwrap();
        g.verify(report.journal_id, "human", None).unwrap();

        let st = g.stats(None).unwrap();
        assert_eq!(st.codex_entries, 1);
        assert_eq!(st.journal_entries, 1);
        assert_eq!(st.scratch_sessions, 0);
        assert_eq!(st.verified, 2);
        assert_eq!(st.unverified, 0);
        assert!((st.verification_rate - 1.0).abs() < 1e-9);

        let scoped = g.stats(Some("builder")).unwrap();
        assert_eq!(scoped.journal_entries, 1);
        let other = g.stats(Some("nobody")).unwrap();
        assert_eq!(other.journal_entries, 0);
    }

    #[test]
    fn stats_stale_excludes_rolled_back_and_superseded_is_codex_only() {
        let (_d, g) = gng();
        let now = chrono::Utc::now();
        let codex = g.codex().unwrap();

        let mut old = Entry::new("a", EntryKind::Decision, "old unverified checkout fact");
        old.timestamp = now - chrono::Duration::days(31);
        codex.create(&old).unwrap();

        let mut ancient = Entry::new("a", EntryKind::Decision, "ancient rolled back fact");
        ancient.timestamp = now - chrono::Duration::days(40);
        ancient.mark_rolled_back("ops");
        codex.create(&ancient).unwrap();

        let v1 = Entry::new("a", EntryKind::Decision, "codex chain tail");
        codex.create(&v1).unwrap();
        let mut v2 = Entry::new("a", EntryKind::Decision, "codex chain head");
        v2.revises = Some(v1.id);
        codex.create(&v2).unwrap();

        let journal = g.journal("builder").unwrap();
        let j1 = Entry::new("builder", EntryKind::Observation, "journal tail");
        journal.create(&j1).unwrap();
        let mut j2 = Entry::new("builder", EntryKind::Observation, "journal head");
        j2.revises = Some(j1.id);
        journal.create(&j2).unwrap();

        let s = g.start_session("builder", "open work").unwrap();
        g.add_observation(&s, "scratch note about checkout")
            .unwrap();

        let mut exactly_30 = Entry::new("a", EntryKind::Decision, "exactly thirty days old");
        exactly_30.timestamp = now - chrono::Duration::days(30);
        codex.create(&exactly_30).unwrap();

        std::fs::create_dir_all(g.root().join(SCRATCH).join("empty-session")).unwrap();

        let st = g.stats(None).unwrap();
        assert_eq!(st.stale_over_30d, 1);
        assert_eq!(st.superseded, 1);
        assert_eq!(st.scratch_sessions, 1);
        assert_eq!(st.rolled_back, 1);
        let decidable = st.verified + st.unverified + st.contradicted;
        assert_eq!(st.verification_rate, st.verified as f64 / decidable as f64);
        assert!(st.rolled_back > 0);
        assert!(decidable > st.verified);
    }

    #[test]
    fn peer_coverage_counts_other_journals_without_leaking_content() {
        let (_d, g) = gng();
        let scout = g.start_session("scout", "fix checkout latency").unwrap();
        g.add_attempt(&scout, "added index for checkout latency", false)
            .unwrap();
        g.end_session(&scout, "index did not help", vec![]).unwrap();
        let mine = g.start_session("builder", "fix checkout latency").unwrap();
        g.add_attempt(&mine, "increased pool size", true).unwrap();
        g.end_session(&mine, "pool resize worked", vec![]).unwrap();

        let pc = g
            .peer_coverage(Some("builder"), "checkout latency")
            .unwrap();
        assert_eq!(pc.agents_attempted, 1);
        assert_eq!(pc.agents_failed, 1);
        assert!(pc.entries >= 1);

        // The requester's own journal is excluded; scout sees builder.
        let pc = g.peer_coverage(Some("scout"), "checkout latency").unwrap();
        assert_eq!(pc.agents_attempted, 1);
        assert_eq!(pc.agents_failed, 0);
        // No scope counts every journal.
        let pc = g.peer_coverage(None, "checkout latency").unwrap();
        assert_eq!(pc.agents_attempted, 2);

        // Counts surface in the briefing; peer text never does.
        let b = g.brief("builder", "checkout latency", 10).unwrap();
        assert!(
            b.markdown
                .contains("1 other agent attempted this topic; 1 failed"),
            "{}",
            b.markdown
        );
        assert!(!b.markdown.contains("index did not help"), "{}", b.markdown);
    }

    #[test]
    fn revalidate_flags_drifted_and_missing_evidence_files() {
        let (d, g) = gng();
        let adr = d.path().join("adr.md");
        std::fs::write(&adr, "decision: use postgres").unwrap();

        let codex = g.codex().unwrap();
        let e = Entry::new("ops", EntryKind::Decision, "use postgres");
        codex.create(&e).unwrap();
        g.verify_with_evidence(e.id, "team", None, std::slice::from_ref(&adr))
            .unwrap();

        // Intact.
        let checks = g.revalidate(e.id).unwrap();
        assert_eq!(checks.len(), 1);
        assert!(checks[0].intact());

        // Drift.
        std::fs::write(&adr, "decision: use sqlite").unwrap();
        let checks = g.revalidate(e.id).unwrap();
        assert!(!checks[0].intact());
        assert_eq!(
            checks[0].actual_sha256.as_deref().map(|s| s.len()),
            Some(64)
        );

        // Missing.
        std::fs::remove_file(&adr).unwrap();
        let checks = g.revalidate(e.id).unwrap();
        assert!(!checks[0].intact());
        assert_eq!(checks[0].actual_sha256, None);
    }

    #[test]
    fn revalidate_all_spans_layers_and_skips_ref_evidence() {
        let (d, g) = gng();
        let adr = d.path().join("spec.md");
        std::fs::write(&adr, "the spec").unwrap();

        let s = g.start_session("builder", "write spec").unwrap();
        g.add_observation(&s, "spec drafted").unwrap();
        let report = g
            .end_session(
                &s,
                "spec done",
                vec![Promotion {
                    kind: EntryKind::Decision,
                    summary: "spec chosen".into(),
                    body: "".into(),
                    review_of: None,
                }],
            )
            .unwrap();
        let promoted = g.codex().unwrap().require(report.promoted[0]).unwrap();
        // Promoted evidence is a Ref to the journal archive; revalidation
        // must skip it and check only file evidence.
        assert!(matches!(promoted.evidence[0], Evidence::Ref { .. }));
        g.verify_with_evidence(report.promoted[0], "team", None, &[adr])
            .unwrap();

        let report_all = g.revalidate_all().unwrap();
        assert_eq!(report_all.checks.len(), 1);
        assert!(report_all.checks[0].intact());
        assert_eq!(report_all.checks[0].entry, report.promoted[0]);
        assert!(report_all.unreadable.is_empty());
    }

    #[test]
    fn peer_coverage_survives_a_corrupt_peer_journal() {
        let (_d, g) = gng();
        let scout = g.start_session("scout", "fix checkout latency").unwrap();
        g.add_attempt(&scout, "retried checkout", false).unwrap();
        let report = g
            .end_session(&scout, "checkout retry failed", vec![])
            .unwrap();

        // Corrupt the session's archive entry inside scout's journal.
        let archive = g.journal("scout").unwrap().path_for(report.journal_id);
        std::fs::write(&archive, "not an entry file").unwrap();

        // Briefing must not die; it just counts what it can read.
        let pc = g
            .peer_coverage(Some("builder"), "checkout latency")
            .unwrap();
        assert_eq!(pc.agents_attempted, 0);
        let b = g.brief("builder", "checkout latency", 10).unwrap();
        assert!(!b.markdown.contains("Peers:"));
    }

    #[test]
    fn peer_coverage_counts_only_topic_matched_failures() {
        // A session matched on the topic must not have its unrelated
        // failures blamed on it: only a failed line that shares stemmed
        // tokens with the topic counts.
        let (_d, g) = gng();
        let scout = g.start_session("scout", "fix checkout latency").unwrap();
        g.add_attempt(&scout, "restart deploy runner", false)
            .unwrap();
        g.add_attempt(&scout, "patch checkout queue", true).unwrap();
        g.end_session(&scout, "checkout fixed, runner still broken", vec![])
            .unwrap();

        let pc = g.peer_coverage(Some("builder"), "checkout").unwrap();
        assert_eq!(pc.agents_attempted, 1);
        assert_eq!(pc.agents_failed, 0);

        // A topic that does match the failed line counts it.
        let pc = g.peer_coverage(Some("builder"), "deploy runner").unwrap();
        assert_eq!(pc.agents_failed, 1);

        // The archive's own wrapper tokens ("attempt", "failed") are
        // formatting, not topic evidence: a topic mentioning failures
        // must not match every failed line through the label alone.
        let pc = g.peer_coverage(Some("builder"), "failed checkout").unwrap();
        assert_eq!(pc.agents_failed, 0);
    }

    #[test]
    fn revalidate_distinguishes_unreadable_from_missing() {
        // A file that exists but can't be read reports its error instead
        // of collapsing into the MISSING outcome.
        let (d, g) = gng();
        let f = d.path().join("adr.md");
        std::fs::write(&f, "decision").unwrap();

        let codex = g.codex().unwrap();
        let e = Entry::new("ops", EntryKind::Decision, "use postgres");
        codex.create(&e).unwrap();
        g.verify_with_evidence(e.id, "team", None, std::slice::from_ref(&f))
            .unwrap();

        // Replace the file with a directory: exists, unreadable, portable
        // across unix and Windows.
        std::fs::remove_file(&f).unwrap();
        std::fs::create_dir(&f).unwrap();
        let checks = g.revalidate(e.id).unwrap();
        assert_eq!(checks.len(), 1);
        assert!(!checks[0].intact());
        assert!(checks[0].read_error.is_some());
    }

    #[test]
    fn session_task_mentioning_failed_does_not_count_as_failure() {
        let (_d, g) = gng();
        let scout = g
            .start_session("scout", "investigate [failed] checkout")
            .unwrap();
        g.add_observation(&scout, "checkout note").unwrap();
        g.end_session(&scout, "read the [failed] reports", vec![])
            .unwrap();

        let pc = g.peer_coverage(Some("builder"), "checkout").unwrap();
        assert_eq!(pc.agents_attempted, 1);
        assert_eq!(pc.agents_failed, 0);
    }

    #[test]
    fn evidence_paths_inside_the_store_are_recorded_relative() {
        // A committed codex must stay portable: paths under the store root
        // are recorded relative to it (absolute would leak the workstation
        // layout and break revalidation on a clone), paths outside it keep
        // the absolute form.
        let (d, g) = gng();
        let inside = d.path().join("adr-inside.md");
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("adr-outside.md");
        std::fs::write(&inside, "decision").unwrap();
        std::fs::write(&outside, "decision").unwrap();

        let codex = g.codex().unwrap();
        let e = Entry::new("ops", EntryKind::Decision, "use postgres");
        codex.create(&e).unwrap();
        g.verify_with_evidence(e.id, "team", None, &[inside, outside.clone()])
            .unwrap();

        let entry = codex.require(e.id).unwrap();
        let mut paths = entry.evidence.iter().map(|ev| match ev {
            Evidence::File { path, .. } => PathBuf::from(path),
            _ => panic!("expected file evidence"),
        });
        assert_eq!(paths.next().unwrap(), PathBuf::from("adr-inside.md"));
        // Outside paths keep the canonicalized absolute form (verbatim
        // `\\?\` and 8.3 names on Windows), not the spelling passed in.
        assert_eq!(
            paths.next().unwrap(),
            std::fs::canonicalize(&outside).unwrap()
        );

        // A root-relative record still revalidates: resolution goes through
        // the store root, not the caller's cwd.
        assert!(g.revalidate(e.id).unwrap().iter().all(|c| c.intact()));
    }

    #[test]
    fn revalidate_all_reports_unreadable_entries_instead_of_aborting() {
        // Same error contract as peer coverage: one corrupt entry file is a
        // reported row, not a reason to hide the rest of the store.
        let (d, g) = gng();
        let adr = d.path().join("adr.md");
        std::fs::write(&adr, "decision").unwrap();

        let codex = g.codex().unwrap();
        let e = Entry::new("ops", EntryKind::Decision, "use postgres");
        codex.create(&e).unwrap();
        g.verify_with_evidence(e.id, "team", None, std::slice::from_ref(&adr))
            .unwrap();

        let corrupt_path = codex.path_for(e.id);
        std::fs::write(&corrupt_path, "not an entry file").unwrap();

        let report = g.revalidate_all().unwrap();
        assert!(report.checks.is_empty());
        assert_eq!(report.unreadable, vec![corrupt_path]);
    }

    #[test]
    fn revalidate_reports_weird_sha256_without_panicking() {
        let (d, g) = gng();
        let f = d.path().join("file.txt");
        std::fs::write(&f, "content").unwrap();

        let codex = g.codex().unwrap();
        let mut e = Entry::new("ops", EntryKind::Decision, "hand-edited hash");
        e.evidence.push(Evidence::File {
            path: f.display().to_string(),
            excerpt: "content".into(),
            sha256: "é-mørk-unicode-nonsense".into(),
        });
        codex.create(&e).unwrap();

        let checks = g.revalidate(e.id).unwrap();
        assert!(!checks[0].intact());
    }

    #[cfg(unix)]
    #[test]
    fn stats_skips_symlinks_and_unsanitized_journal_dirs() {
        let (_d, g) = gng();
        let journal = g.journal("builder").unwrap();
        let e = Entry::new("builder", EntryKind::Observation, "real journal note");
        journal.create(&e).unwrap();

        let journal_base = g.root().join(JOURNAL);
        std::os::unix::fs::symlink("/tmp", journal_base.join("link")).unwrap();
        std::fs::create_dir_all(journal_base.join("bad!")).unwrap();

        let st = g.stats(None).unwrap();
        assert_eq!(st.journal_entries, 1);
        assert!(g.locate(e.id).unwrap().is_some());
    }
}
