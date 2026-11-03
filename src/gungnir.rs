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

use crate::briefing::{self, Briefing, BriefingInput};
use crate::entry::{Entry, EntryKind, Evidence, VerificationState};
use crate::id::EntryId;
use crate::layout::{self, CODEX, JOURNAL, SCRATCH};
use crate::recall::{self, Hit, Query, SearchOutcome};
use crate::rollback;
use crate::store::Skipped;
use crate::validate::MAX_SUMMARY_LEN;
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
}

#[derive(Clone, Debug)]
pub struct Gungnir {
    root: PathBuf,
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
            let promoted =
                self.finish_promotions(archive, &session.id, &session.agent, promotions)?;
            if scratch_exists {
                std::fs::remove_dir_all(&dir)?;
            }
            return Ok(EndReport {
                journal_id: archive,
                promoted,
                already_ended: true,
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
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }

        let promoted =
            self.finish_promotions(journal_entry.id, &session.id, &session.agent, promotions)?;

        Ok(EndReport {
            journal_id: journal_entry.id,
            promoted,
            already_ended: false,
        })
    }

    /// Write session promotions into the Codex, each citing the archive as
    /// evidence. Retries dedupe on `session_id` + `summary`, so a crash or
    /// validation failure mid-loop leaves the next end_session able to
    /// finish the set without duplicating what landed.
    fn finish_promotions(
        &self,
        archive: EntryId,
        session_id: &str,
        agent: &str,
        promotions: Vec<Promotion>,
    ) -> Result<Vec<EntryId>> {
        let codex = self.codex()?;
        // Dedupe on (kind, summary, body): two different findings can
        // share a summary, and a retry re-listing a promotion must land
        // it rather than alias the landed one — but only when the body
        // also matches, so distinct findings keep their own text.
        let mut by_key: HashMap<(EntryKind, String, String), EntryId> = codex
            .entries()?
            .into_iter()
            .filter(|e| e.session_id.as_deref() == Some(session_id))
            .map(|e| ((e.kind, e.summary, e.body), e.id))
            .collect();
        let mut promoted_ids = Vec::new();
        for p in promotions {
            if let Some(&existing) = by_key.get(&(p.kind, p.summary.clone(), p.body.clone())) {
                promoted_ids.push(existing);
                continue;
            }
            let mut c = Entry::new(agent, p.kind, p.summary);
            c.session_id = Some(session_id.to_string());
            c.body = p.body;
            c.review_of = p.review_of;
            c.evidence.push(Evidence::Ref { id: archive });
            codex.create_with(&c, &|id| self.exists_anywhere(id))?;
            by_key.insert((c.kind, c.summary.clone(), c.body.clone()), c.id);
            promoted_ids.push(c.id);
        }
        Ok(promoted_ids)
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
        }))
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
        entry.contradict(by, verifier, note);
        store.update_with(&entry, &|id| self.exists_anywhere(id))
    }

    /// Write a revision of `old`: same kind, project, and evidence, linked
    /// by `revises`. The parent's evidence carries forward — a revision of
    /// a fact still stands on the record that proved it.
    pub fn supersede(
        &self,
        old: EntryId,
        agent: &str,
        summary: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<EntryId> {
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
        Ok(next.id)
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
    /// file that `end_session` deletes.
    pub fn promote(
        &self,
        from: EntryId,
        agent: &str,
        kind: EntryKind,
        summary: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<EntryId> {
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
        Ok(c.id)
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
            .unwrap();
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
            .unwrap();
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
