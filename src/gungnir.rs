//! The Gungnir facade: high-level operations over the three layers.
//!
//! Layer mapping (see [`crate::layout`]):
//! - Codex: `<root>/codex` — shared source of truth
//! - Journal: `<root>/journal/<agent>` — private per-agent history
//! - Scratch: `<root>/scratch/<session>` — ephemeral per-task memory

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::briefing::{self, Briefing};
use crate::entry::{Entry, EntryKind, Evidence};
use crate::id::EntryId;
use crate::layout::{self, CODEX, JOURNAL, SCRATCH};
use crate::recall::{self, Hit, Query};
use crate::rollback;
use crate::validate::MAX_SUMMARY_LEN;
use crate::{Error, Result, Store};

/// One open working session. Cheap value; nothing hits disk until the first
/// observation lands.
#[derive(Clone, Debug)]
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
}

#[derive(Clone, Debug)]
pub struct EndReport {
    pub journal_id: EntryId,
    pub promoted: Vec<EntryId>,
}

#[derive(Clone, Debug)]
pub struct Gungnir {
    root: PathBuf,
}

impl Gungnir {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        for sub in [CODEX, JOURNAL, SCRATCH] {
            std::fs::create_dir_all(root.join(sub))?;
        }
        Ok(Self { root })
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
        Store::open(self.root.join(JOURNAL).join(layout::sanitize_component(agent)))
    }

    pub fn scratch(&self, session_id: &str) -> Result<Store> {
        Store::open(self.root.join(SCRATCH).join(layout::sanitize_component(session_id)))
    }

    // -- session lifecycle ---------------------------------------------------

    pub fn start_session(&self, agent: impl Into<String>, task: impl Into<String>) -> Session {
        Session {
            id: EntryId::generate().to_string(),
            agent: agent.into(),
            task: task.into(),
            started_at: Utc::now(),
        }
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
        let mut entry = Entry::new(&session.agent, kind, summarize(&text));
        entry.session_id = Some(session.id.clone());
        entry.body = match outcome {
            Some(ok) => format!("outcome: {}\n\n{}", if ok { "succeeded" } else { "failed" }, text),
            None => text,
        };
        self.scratch(&session.id)?.create(&entry)?;
        Ok(entry.id)
    }

    /// Archive the session into the Journal, promote findings into the Codex,
    /// then clear the scratch directory. Idempotent-friendly: re-ending an
    /// already-ended session errors on the missing scratch dir only after
    /// the journal write succeeded, so callers can safely retry promotion.
    pub fn end_session(
        &self,
        session: &Session,
        summary: impl AsRef<str>,
        promotions: Vec<Promotion>,
    ) -> Result<EndReport> {
        let summary = summary.as_ref();
        let scratch = self.scratch(&session.id)?;
        let mut transcript = String::new();
        for e in scratch.entries()? {
            let outcome = if e.body.starts_with("outcome: ") {
                format!(" [{}]", e.body["outcome: ".len()..].lines().next().unwrap_or(""))
            } else {
                String::new()
            };
            transcript.push_str(&format!("- {}: {}{}\n", e.kind, e.summary, outcome));
        }

        let mut journal_entry = Entry::new(&session.agent, EntryKind::Session, summary);
        journal_entry.session_id = Some(session.id.clone());
        journal_entry.body = format!(
            "# Task\n{}\n\n# Transcript\n{}",
            session.task, transcript
        );
        let journal_store = self.journal(&session.agent)?;
        journal_store.create(&journal_entry)?;

        let codex = self.codex()?;
        let mut promoted_ids = Vec::new();
        let exists = |id: EntryId| -> crate::Result<bool> {
            Ok(codex.exists(id)? || journal_store.exists(id)? || scratch.exists(id)?)
        };
        for p in promotions {
            let mut c = Entry::new(&session.agent, p.kind, p.summary);
            c.session_id = Some(session.id.clone());
            c.body = p.body;
            c.evidence.push(Evidence::Ref { id: journal_entry.id });
            codex.create_with(&c, &exists)?;
            promoted_ids.push(c.id);
        }

        let dir = self.root.join(SCRATCH).join(layout::sanitize_component(&session.id));
        if dir.exists() {
            std::fs::remove_dir_all(&dir)?;
        }

        Ok(EndReport { journal_id: journal_entry.id, promoted: promoted_ids })
    }

    // -- reading -------------------------------------------------------------

    /// Compile the pre-task briefing: Codex facts plus this agent's prior
    /// attempts relevant to `task`.
    pub fn brief(&self, agent: &str, task: &str, limit: usize) -> Result<Briefing> {
        let codex = self.codex()?;
        let journal = self.journal(agent)?;
        let q = Query::new(task, limit);

        let codex_hits = recall::search(&codex, &q)?;
        let journal_hits = recall::search(&journal, &q)?;
        Ok(briefing::compile(
            task,
            codex_hits,
            journal_hits,
            superseded_ids(&codex)?,
            superseded_ids(&journal)?,
        ))
    }

    /// Keyword recall restricted to one layer.
    pub fn recall_layer(&self, layer: Layer, query: &Query) -> Result<Vec<Hit>> {
        match layer {
            Layer::Codex => recall::search(&self.codex()?, query),
            Layer::Journal { agent } => recall::search(&self.journal(agent)?, query),
            Layer::Scratch { session_id } => recall::search(&self.scratch(session_id)?, query),
        }
    }

    // -- mutations ------------------------------------------------------------

    /// Reference resolution across every layer. Evidence may point anywhere
    /// (Codex fact citing its Journal archive, etc.), so all mutation paths
    /// validate against the union.
    fn exists_anywhere(&self, id: EntryId) -> Result<bool> {
        if self.codex()?.exists(id)? {
            return Ok(true);
        }
        for base_name in [JOURNAL, SCRATCH] {
            let base = self.root.join(base_name);
            if !base.exists() {
                continue;
            }
            for sub in std::fs::read_dir(&base)? {
                if Store::open(base.join(sub?.file_name()))?.exists(id)? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn verify(&self, id: EntryId, verifier: &str, note: Option<String>) -> Result<()> {
        let store = self.locate(id)?.ok_or(Error::NotFound(id))?;
        let mut entry = store.require(id)?;
        entry.verify(verifier, note);
        store.update_with(&entry, &|id| self.exists_anywhere(id))
    }

    pub fn contradict(&self, id: EntryId, by: EntryId, verifier: &str) -> Result<()> {
        let store = self.locate(id)?.ok_or(Error::NotFound(id))?;
        if !self.exists_anywhere(by)? {
            return Err(Error::NotFound(by));
        }
        let mut entry = store.require(id)?;
        entry.contradict(by, verifier);
        store.update_with(&entry, &|id| self.exists_anywhere(id))
    }

    /// Write a revision of `old`: same kind and project, linked by `revises`.
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
        next.body = body.into();
        store.create_with(&next, &|id| self.exists_anywhere(id))?;
        Ok(next.id)
    }

    pub fn rollback(&self, target: EntryId, agent: &str) -> Result<EntryId> {
        let store = self.locate(target)?.ok_or(Error::NotFound(target))?;
        rollback::rollback(&store, target, agent)
    }

    /// Promote an existing entry's finding into the Codex with a provenance
    /// link back to `from`. Works across layers (e.g. Journal archive as
    /// evidence for a Codex fact).
    pub fn promote(
        &self,
        from: EntryId,
        agent: &str,
        kind: EntryKind,
        summary: impl Into<String>,
        body: impl Into<String>,
    ) -> Result<EntryId> {
        let source = self.locate(from)?.ok_or(Error::NotFound(from))?;
        let codex = self.codex()?;
        let mut c = Entry::new(agent, kind, summary);
        c.body = body.into();
        c.evidence.push(Evidence::Ref { id: from });
        let exists = |id: EntryId| -> Result<bool> {
            Ok(codex.exists(id)? || source.exists(id)?)
        };
        codex.create_with(&c, &exists)?;
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
    if !base.exists() {
        return Ok(None);
    }
    for agent_dir in std::fs::read_dir(base)? {
        let dir = base.join(agent_dir?.file_name());
        if Store::open(&dir)?.exists(id)? {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}

/// Ids that some newer entry revises, i.e. facts with a newer version.
pub fn superseded_ids(store: &Store) -> Result<HashSet<EntryId>> {
    Ok(store
        .entries()?
        .into_iter()
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
        let s = g.start_session("builder", "fix slow checkout query");
        g.add_observation(&s, "EXPLAIN shows seq scan on orders_archive").unwrap();
        g.add_attempt(&s, "added index on orders.user_id", false).unwrap();
        g.add_attempt(&s, "rewrote query to use orders_archive_idx", true).unwrap();

        let report = g
            .end_session(
                &s,
                "rewrote checkout query to use orders_archive_idx",
                vec![Promotion {
                    kind: EntryKind::Decision,
                    summary: "checkout queries must use orders_archive_idx".into(),
                    body: "seq scan on orders_archive was the bottleneck".into(),
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
            vec![Evidence::Ref { id: report.journal_id }]
        );

        // Scratch cleared.
        assert!(g.scratch(&s.id).unwrap().entries().unwrap().is_empty()
            || !g.root.join(SCRATCH).join(layout::sanitize_component(&s.id)).exists());
    }

    #[test]
    fn briefing_surfaces_codex_and_own_journal_only() {
        let (_d, g) = gng();

        // Another agent's failure must NOT appear in builder's briefing.
        let other = g.start_session("scout", "checkout perf");
        g.add_attempt(&other, "tried vacuum full on orders", false).unwrap();
        g.end_session(&other, "vacuum did not help", vec![]).unwrap();

        let mine = g.start_session("builder", "fix slow checkout");
        g.add_attempt(&mine, "added checkout cache", true).unwrap();
        g.end_session(&mine, "cache helped", vec![]).unwrap();

        let b = g.brief("builder", "slow checkout", 10).unwrap();
        assert!(b.markdown.contains("checkout cache"), "{}", b.markdown);
        assert!(!b.markdown.contains("vacuum"), "{}", b.markdown);
    }

    #[test]
    fn supersede_links_and_locate_finds_layers() {
        let (_d, g) = gng();
        let codex = g.codex().unwrap();
        let v1 = Entry::new("a", EntryKind::Decision, "use mysql");
        codex.create(&v1).unwrap();

        let v2 = g.supersede(v1.id, "a", "use postgres", "migration done").unwrap();
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

        g.verify(e.id, "ops", Some("checked dashboard".into())).unwrap();
        let after = g.locate(e.id).unwrap().unwrap().require(e.id).unwrap();
        assert_eq!(after.verification, crate::entry::VerificationState::Verified);
        assert_eq!(after.verification_log.len(), 1);
    }

    #[test]
    fn verify_works_on_promoted_entries_with_cross_layer_evidence() {
        // Regression: update-path validation used to resolve evidence refs
        // against one store only, so verifying a Codex entry whose evidence
        // cites its Journal archive failed.
        let (_d, g) = gng();
        let s = g.start_session("builder", "tune cache");
        g.add_observation(&s, "cache hit rate 12%").unwrap();
        let report = g.end_session(&s, "raised hit rate", vec![]).unwrap();

        let cid = g
            .promote(report.journal_id, "builder", EntryKind::Decision,
                     "cache tuning worked", "hit rate now 40%")
            .unwrap();
        g.verify(cid, "team", None).unwrap();

        let stored = g.codex().unwrap().require(cid).unwrap();
        assert_eq!(stored.verification, crate::entry::VerificationState::Verified);
    }
}
