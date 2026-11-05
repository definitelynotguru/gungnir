//! Conflict candidates: the write-time half of the contradiction loop.
//!
//! When a write lands next to a *live verified* claim on the same topic,
//! the store records a conflict candidate under `<root>/.conflicts/` and the
//! write still succeeds — candidates are advisory, never a gate. Detection
//! is deterministic retrieval plus lexical heuristics; resolution is a
//! logged human/agent decision (`supersede`, `contradict`, `dismiss`).
//! Nothing LLM-judged enters the write path.
//!
//! Detection is deliberately conservative on the trust side: it flags a new
//! claim that shares a topic frame with a verified fact while asserting
//! something different, and lets people dismiss the noise. A missed
//! candidate is silent; a false positive costs one `resolve --dismiss`.
//!
//! The signals (all lexical, computed over stemmed summary tokens):
//!
//! - `shared_frame`: the claims share ≥2 content tokens (the topic) *and*
//!   each carries private tokens (the claims diverge). "sessions use mysql"
//!   vs "sessions use postgres" shares {session, use} and diverges on the
//!   value. Near-duplicates are excluded because an identical token set
//!   leaves nothing private.
//! - `negation_asymmetry`: shared topic, but exactly one side negates
//!   ("don't use postgres" vs "use postgres"). Also catches
//!   reworded-negation conflicts where the shared frame is thinner.
//! - `numeric_mismatch`: shared topic, but the digit-bearing tokens differ
//!   ("timeout is 30s" vs "timeout is 60s").
//!
//! Semantic mode (`embed` feature + an [`Embedder`]) adds a fourth channel:
//! a verified live entry with cosine ≥ `SEMANTIC_CONFLICT_FLOOR` is a
//! candidate even with no lexical signal, which catches paraphrased
//! contradictions ("checkout is slow" vs "checkout is fast"). Its
//! false-positive rate is higher — same-topic corroborations also sit
//! close — so it is opt-in via the `--semantic` flag, not the default.
//!
//! Only claim-shaped kinds participate (`Session` archives and `Rollback`
//! records are bookkeeping, not claims), and only live verified entries are
//! candidates: superseded tails, contradicted, and rolled-back facts are
//! excluded by the same `current` temporal view recall uses.
//!
//! Why a `.conflicts/` directory and not virtual state derived from scans:
//! a candidate has a lifecycle. `dismissed` and `resolved-*` are terminal
//! states no scan can reconstruct — they must persist, and a markdown file
//! per candidate keeps the queue git-diffable and human-editable like the
//! rest of the store. Files are the record; nothing is derived.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::embedding::{self, Embedder};
use crate::entry::{Entry, EntryKind, Evidence, VerificationState};
use crate::error::{Error, Result};
use crate::id::EntryId;
use crate::recall::{self, Query, StateAt};
use crate::Store;

/// Candidates scanned per write. Verified hits sort first under the
/// bucket-first ordering, so the cap only sheds unverified noise.
const CANDIDATE_LIMIT: usize = 64;

/// Cosine floor for the semantic channel: "same claim, different words".
/// Higher than the recall match floor (0.45) because a conflict candidate
/// must be about the *same* claim, not merely the same topic.
const SEMANTIC_CONFLICT_FLOOR: f32 = 0.70;

/// Raw (pre-stem) tokens that flip a claim's truth value. Matched before
/// stemming because the stemmer collapses ordinary words into negations:
/// "noted"/"noting" stem to `not`. Apostrophes split contractions, so
/// "don't" still arrives as `don` and "won't" as `won` (kept: the
/// contraction dwarfs the past-tense verb in claim text). `can` is
/// deliberately absent — "can't" tokenizes to `can`, indistinguishable
/// from positive "can", a documented blind spot.
const NEGATIONS: &[&str] = &[
    "no", "not", "never", "cannot", "cant", "dont", "don", "wont", "won", "isnt", "isn", "arent",
    "aren", "wasnt", "wasn", "werent", "weren", "didnt", "didn", "doesnt", "doesn", "couldnt",
    "couldn", "shouldnt", "shouldn", "mustnt", "mustn", "neednt", "needn", "hasnt", "hasn",
    "hadnt", "hadn", "aint", "ain", "shant", "shan",
];

/// Lowercase alphanumeric tokens as written — the shape negations have
/// before the tokenizer's stopword filter and stemmer run.
fn raw_tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect()
}

/// Unique id for a conflict-candidate record. ULID-backed like [`EntryId`]
/// but a separate type: a candidate id is not an entry id, and passing one
/// where the other is expected is a type error, not a silent miss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConflictId(Ulid);

impl ConflictId {
    pub fn generate() -> Self {
        Self(Ulid::new())
    }
}

impl std::fmt::Display for ConflictId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::str::FromStr for ConflictId {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ulid::from_string(s).map(Self)
    }
}

impl Serialize for ConflictId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ConflictId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// The deterministic signal that raised a candidate. Serialized so the
/// record says *why* it exists, not just that it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictSignal {
    SharedFrame,
    NegationAsymmetry,
    NumericMismatch,
    SemanticOverlap,
}

/// Candidate lifecycle. `Open` awaits a resolution verb; the rest are
/// terminal and record which verb closed the loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ConflictStatus {
    Open,
    ResolvedSuperseded,
    ResolvedContradicted,
    Dismissed,
}

impl std::fmt::Display for ConflictStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Open => "open",
            Self::ResolvedSuperseded => "resolved-superseded",
            Self::ResolvedContradicted => "resolved-contradicted",
            Self::Dismissed => "dismissed",
        };
        f.write_str(s)
    }
}

/// The resolution verb a caller asks `resolve` to apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictVerb {
    /// The new entry is the newer truth: link `new_entry.revises` to the
    /// conflicting entry so supersession hides it under `current` views.
    Supersede,
    /// The verified entry is wrong: mark it `contradicted_by` the new one.
    Contradict,
    /// Not actually a conflict: close the candidate, touch nothing else.
    Dismiss,
}

/// One recorded conflict candidate. Frontmatter carries the machine state;
/// the body below it is a rendered view for humans reading the file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConflictRecord {
    pub id: ConflictId,
    /// Layer the conflicting entry lives in ("codex", "journal/<agent>").
    pub layer: String,
    /// The new claim that triggered detection.
    pub new_entry: EntryId,
    /// The live verified claim it may conflict with.
    pub conflicting_entry: EntryId,
    /// Summary snapshots: audit survives later edits to either entry.
    pub new_summary: String,
    pub conflicting_summary: String,
    pub signals: Vec<ConflictSignal>,
    pub detected_at: DateTime<Utc>,
    #[serde(flatten)]
    pub status: ConflictStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
}

impl ConflictRecord {
    /// Human-readable body written under the frontmatter.
    fn render(&self) -> String {
        let signals = self
            .signals
            .iter()
            .map(|s| {
                serde_json::to_value(s)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{s:?}"))
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "New:        \"{}\" ({})\nConflicts:  \"{}\" ({}) in {}\nSignals:    {}\nStatus:     {}",
            self.new_summary,
            self.new_entry,
            self.conflicting_summary,
            self.conflicting_entry,
            self.layer,
            signals,
            self.status,
        )
    }
}

/// The `.conflicts/` queue: one markdown file per candidate, same
/// durability contract as [`Store`] — temp file + atomic rename, flock for
/// multi-write serialization. `open` never creates the directory; reads on
/// a store that has no candidates return empty lists.
#[derive(Clone, Debug)]
pub struct Conflicts {
    dir: PathBuf,
}

impl Conflicts {
    /// Open never creates the directory — reads on a store with no
    /// candidates return empty lists. It does sweep temp siblings left by
    /// crashed writes, matching the store's durability contract.
    pub fn open(dir: impl Into<PathBuf>) -> Self {
        let this = Self { dir: dir.into() };
        this.sweep_stale_temps();
        this
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Remove orphaned `.tmp-*` siblings from crashed writes older than a
    /// minute. Best-effort: a sweep failure must not stop a read.
    fn sweep_stale_temps(&self) {
        let Ok(dents) = fs::read_dir(&self.dir) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for dent in dents.flatten() {
            let stale = dent.file_name().to_string_lossy().starts_with(".tmp-")
                && fs::metadata(dent.path())
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|mtime| now.duration_since(mtime).ok())
                    .is_some_and(|age| age > std::time::Duration::from_secs(60));
            if stale {
                let _ = fs::remove_file(dent.path());
            }
        }
    }

    fn path_for(&self, id: ConflictId) -> PathBuf {
        self.dir.join(format!("{id}.md"))
    }

    /// Persist a new candidate. Called from the write path after the entry
    /// has landed, so callers turn a failure into a warning — the entry
    /// stands either way and candidates are advisory. Returns the existing
    /// record when one already covers the same (new, conflicting) pair —
    /// open or settled — which makes rescans (end_session retries in
    /// particular) idempotent: one disagreement keeps one record, and a
    /// settled pair stays settled instead of reopening. The dedupe scan is
    /// lenient on malformed records: a corrupt file can never stand in for
    /// the pair, so it is skipped here and stays visible on the strict
    /// audit surfaces (`list`, `list_open`, `conflicts`).
    pub fn record(&self, rec: &ConflictRecord) -> Result<Option<ConflictRecord>> {
        fs::create_dir_all(&self.dir)?;
        let _guard = self.lock()?;
        for id in self.ids()? {
            let Some(existing) = self.get(id).ok().flatten() else {
                continue;
            };
            if existing.new_entry == rec.new_entry
                && existing.conflicting_entry == rec.conflicting_entry
            {
                return Ok(Some(existing));
            }
        }
        self.persist_locked(rec)?;
        Ok(None)
    }

    /// Overwrite an existing record (resolution bookkeeping). Test-only:
    /// production closes records through `resolve_locked`, which serializes
    /// the status recheck — a public blind update could overwrite a
    /// resolution it never saw.
    #[cfg(test)]
    pub fn update(&self, rec: &ConflictRecord) -> Result<()> {
        if self.get(rec.id)?.is_none() {
            return Err(Error::Invalid(format!(
                "conflict {} does not exist",
                rec.id
            )));
        }
        let _guard = self.lock()?;
        self.persist_locked(rec)
    }

    /// Resolve `id` under one lock: re-read, require Open, run `mutate`
    /// (the caller's entry edits), then persist the closure. Two resolvers
    /// can no longer both observe Open and leave a record that says
    /// dismissed over an entry that was superseded. `mutate` must not call
    /// back into the queue — the flock is held and is not reentrant.
    pub fn resolve_locked(
        &self,
        id: ConflictId,
        mutate: impl FnOnce(&ConflictRecord) -> Result<ConflictRecord>,
    ) -> Result<ConflictRecord> {
        fs::create_dir_all(&self.dir)?;
        let _guard = self.lock()?;
        let rec = self.require(id)?;
        if rec.status != ConflictStatus::Open {
            return Err(Error::Invalid(format!(
                "conflict {id} is already {status}",
                status = rec.status
            )));
        }
        let rec = mutate(&rec)?;
        self.persist_locked(&rec)?;
        Ok(rec)
    }

    pub fn get(&self, id: ConflictId) -> Result<Option<ConflictRecord>> {
        let path = self.path_for(id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(parse_record_file(&path)?))
    }

    pub fn require(&self, id: ConflictId) -> Result<ConflictRecord> {
        self.get(id)?
            .ok_or_else(|| Error::Invalid(format!("conflict not found: {id}")))
    }

    /// Candidate ids on disk, oldest first. Absent directory means none.
    fn ids(&self) -> Result<Vec<ConflictId>> {
        let mut ids = Vec::new();
        if self.dir.exists() {
            for dent in fs::read_dir(&self.dir)? {
                let dent = dent?;
                let path = dent.path();
                if !path.is_file() || path.extension() != Some(std::ffi::OsStr::new("md")) {
                    continue;
                }
                if let Ok(id) = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .parse::<ConflictId>()
                {
                    ids.push(id);
                }
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// All candidates, oldest first. Malformed records surface as errors:
    /// this is the audit surface, where corruption must be visible.
    pub fn list(&self) -> Result<Vec<ConflictRecord>> {
        self.ids()?.into_iter().map(|id| self.require(id)).collect()
    }

    /// Open candidates, tolerating every read failure: unparseable records
    /// are skipped and an unreadable queue directory reads as empty.
    /// Briefings read through this — a corrupt or missing queue degrades
    /// the warning, never the briefing; `conflicts`/`resolve` stay strict
    /// and report it.
    pub fn open_lenient(&self) -> Result<Vec<ConflictRecord>> {
        Ok(self
            .ids()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| self.get(id).ok().flatten())
            .filter(|c| c.status == ConflictStatus::Open)
            .collect())
    }

    pub fn list_open(&self) -> Result<Vec<ConflictRecord>> {
        Ok(self
            .list()?
            .into_iter()
            .filter(|c| c.status == ConflictStatus::Open)
            .collect())
    }

    fn persist_locked(&self, rec: &ConflictRecord) -> Result<()> {
        let path = self.path_for(rec.id);
        let frontmatter = serde_yaml::to_string(rec).map_err(|source| Error::Yaml {
            path: path.clone(),
            source,
        })?;
        let file = format!("---\n{frontmatter}---\n\n{}\n", rec.render());
        let tmp = path.with_file_name(format!(".tmp-{}-{}", std::process::id(), rec.id));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(file.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn lock(&self) -> Result<fs::File> {
        fs::create_dir_all(&self.dir)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.dir.join(".lock"))?;
        file.lock_exclusive()?;
        Ok(file)
    }
}

fn parse_record_file(path: &Path) -> Result<ConflictRecord> {
    let raw = fs::read_to_string(path)?;
    let rest = raw
        .strip_prefix("---\n")
        .ok_or_else(|| Error::Invalid(format!("{}: missing frontmatter opener", path.display())))?;
    let (yaml, _body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| Error::Invalid(format!("{}: missing frontmatter closer", path.display())))?;
    serde_yaml::from_str(yaml).map_err(|source| Error::Yaml {
        path: path.into(),
        source,
    })
}

/// Whether an entry asserts a claim worth checking. Session archives and
/// rollback records are bookkeeping; they never raise and never get raised.
pub fn detectable(entry: &Entry) -> bool {
    !matches!(entry.kind, EntryKind::Session | EntryKind::Rollback)
}

/// Ids a candidate must never point at: the new entry itself, the parent it
/// supersedes, the entry it reviews, and the entries it cites as evidence.
fn excluded_ids(entry: &Entry) -> HashSet<EntryId> {
    let mut excluded = HashSet::new();
    excluded.insert(entry.id);
    if let Some(prev) = entry.revises {
        excluded.insert(prev);
    }
    if let Some(target) = entry.review_of {
        excluded.insert(target);
    }
    for ev in &entry.evidence {
        if let Evidence::Ref { id } = ev {
            excluded.insert(*id);
        }
    }
    excluded
}

fn digit_tokens(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().any(|c| c.is_ascii_digit()) && t.len() > 1)
        .map(|t| t.to_lowercase())
        .collect()
}

/// The lexical conflict signals between two claim texts, stemmed-token
/// based. Empty means "no reason to suspect a conflict" — the claims either
/// don't share a topic or say the same thing.
fn lexical_signals(new: &str, old: &str) -> Vec<ConflictSignal> {
    let new_t: HashSet<String> = recall::tokenize(new).into_iter().collect();
    let old_t: HashSet<String> = recall::tokenize(old).into_iter().collect();
    let shared: HashSet<&String> = new_t.intersection(&old_t).collect();
    if shared.is_empty() || new_t == old_t {
        return vec![];
    }
    let mut signals = Vec::new();
    let new_private = new_t.difference(&old_t).next().is_some();
    let old_private = old_t.difference(&new_t).next().is_some();
    if shared.len() >= 2 && new_private && old_private {
        signals.push(ConflictSignal::SharedFrame);
    }
    // Negation is decided on raw tokens: the stemmer turns "noted" into
    // `not`, so stemmed sets would read acknowledgment words as negations.
    let new_raw = raw_tokens(new);
    let old_raw = raw_tokens(old);
    let new_neg = NEGATIONS.iter().any(|n| new_raw.contains(*n));
    let old_neg = NEGATIONS.iter().any(|n| old_raw.contains(*n));
    if new_neg != old_neg {
        signals.push(ConflictSignal::NegationAsymmetry);
    }
    let new_nums = digit_tokens(new);
    let old_nums = digit_tokens(old);
    if !new_nums.is_empty() && !old_nums.is_empty() && new_nums != old_nums {
        signals.push(ConflictSignal::NumericMismatch);
    }
    signals.sort();
    signals
}

/// Scan `store` for live verified entries the new claim may contradict.
/// Keyword channel always runs; with `embedder` the semantic channel adds
/// paraphrase-level matches lexical signals miss. `layer` labels the
/// resulting records ("codex", "journal/<agent>").
pub fn detect(
    store: &Store,
    layer: &str,
    entry: &Entry,
    embedder: Option<&dyn Embedder>,
) -> Result<Vec<ConflictRecord>> {
    detect_many(store, layer, std::slice::from_ref(entry), embedder)
}

/// `detect` for several claims in one pass: the temporal pool, corpus
/// stats, and the embedder call are built once for all `entries`, so
/// `end_session` promoting N claims pays one store scan instead of N.
pub fn detect_many(
    store: &Store,
    layer: &str,
    entries: &[Entry],
    embedder: Option<&dyn Embedder>,
) -> Result<Vec<ConflictRecord>> {
    let entries: Vec<&Entry> = entries.iter().filter(|e| detectable(e)).collect();
    if entries.is_empty() {
        return Ok(vec![]);
    }
    let excluded: Vec<HashSet<EntryId>> = entries.iter().map(|e| excluded_ids(e)).collect();

    // One `current`-view pool for both channels: `state`/`revised` hide
    // superseded tails and contradicted facts from candidacy.
    let q = Query::new("", CANDIDATE_LIMIT).current();
    let (pool, revised, _skipped) = recall::temporal_pool(store, &q)?;
    let stats = recall::CorpusStats::new(pool.iter().map(|c| &c.entry));

    // Semantic vectors: every pool text, then one per new claim — a single
    // embed call covers the batch.
    let vectors = match embedder {
        None => None,
        Some(embedder) => {
            let mut texts: Vec<String> = pool
                .iter()
                .map(|c| format!("{}\n{}", c.entry.summary, c.entry.body))
                .collect();
            texts.extend(entries.iter().map(|e| format!("{}\n{}", e.summary, e.body)));
            let vectors = embedder.embed(&texts)?;
            // The trait documents no length contract; a count that differs
            // in either direction would misalign candidates with vectors
            // or panic.
            if vectors.len() != texts.len() {
                return Err(Error::Invalid(format!(
                    "embedder returned {} vectors for {} texts",
                    vectors.len(),
                    texts.len()
                )));
            }
            Some(vectors)
        }
    };

    let now = Utc::now();
    let mut records = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let excluded = &excluded[i];
        let eligible = |cand: &recall::Candidate| {
            cand.state == StateAt::Verified
                && !revised.contains(&cand.entry.id)
                && detectable(&cand.entry)
                && cand.entry.verification == VerificationState::Verified
                && !excluded.contains(&cand.entry.id)
        };

        let mut signals_by_id: BTreeMap<EntryId, Vec<ConflictSignal>> = BTreeMap::new();
        // Record order for the candidate cap: keyword hits by relevance
        // first, then semantic-only matches in pool order.
        let mut order: Vec<EntryId> = Vec::new();
        let mut summaries: HashMap<EntryId, String> = HashMap::new();

        // Keyword channel: BM25-lite topic match over the same pool,
        // ranked the way recall's `finish` ranks its current view —
        // recency-weighted score, id tie-break — so the candidate cap
        // keeps the conflicts recall would surface first instead of
        // tied facts in oldest-first pool order.
        let mut q_tokens = recall::tokenize(&format!("{}\n{}", entry.summary, entry.body));
        q_tokens.sort();
        q_tokens.dedup();
        let mut scored: Vec<(f64, &recall::Candidate)> = pool
            .iter()
            .filter(|c| eligible(c))
            .map(|c| {
                (
                    recall::score(&c.entry, &q_tokens, &stats)
                        * recall::recency_weight(&c.entry, now),
                    c,
                )
            })
            .filter(|(s, _)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.entry.id.cmp(&b.1.entry.id)));
        for (_, cand) in scored {
            let signals = lexical_signals(&entry.summary, &cand.entry.summary);
            summaries.insert(cand.entry.id, cand.entry.summary.clone());
            if signals.is_empty() {
                continue;
            }
            order.push(cand.entry.id);
            signals_by_id.insert(cand.entry.id, signals);
        }

        // Semantic channel: cosine against this claim's vector. A
        // paraphrased contradiction has no shared tokens; vectors still
        // put it next to the claim it disputes.
        if let Some(vectors) = &vectors {
            let qv = &vectors[pool.len() + i];
            let new_t: HashSet<String> =
                recall::tokenize(&format!("{}\n{}", entry.summary, entry.body))
                    .into_iter()
                    .collect();
            for (cand, vec) in pool.iter().zip(&vectors[..pool.len()]) {
                if !eligible(cand) {
                    continue;
                }
                if embedding::cosine(qv, vec) < SEMANTIC_CONFLICT_FLOOR {
                    continue;
                }
                // Same guard lexical_signals applies on the keyword
                // channel: an identical stemmed token set is
                // corroboration, never a candidate — identical text
                // always embeds above the floor. Compared over
                // summary + body, the text that was embedded: a
                // same-summary pair whose bodies disagree can be a
                // real contradiction the keyword channel can't see.
                let cand_t: HashSet<String> =
                    recall::tokenize(&format!("{}\n{}", cand.entry.summary, cand.entry.body))
                        .into_iter()
                        .collect();
                if cand_t == new_t {
                    continue;
                }
                summaries
                    .entry(cand.entry.id)
                    .or_insert_with(|| cand.entry.summary.clone());
                let mut signals = lexical_signals(&entry.summary, &cand.entry.summary);
                signals.push(ConflictSignal::SemanticOverlap);
                let existing = signals_by_id.entry(cand.entry.id).or_default();
                if existing.is_empty() {
                    order.push(cand.entry.id);
                }
                for s in signals {
                    if !existing.contains(&s) {
                        existing.push(s);
                    }
                }
                existing.sort();
            }
        }

        records.extend(
            order
                .into_iter()
                .take(CANDIDATE_LIMIT)
                .filter_map(|id| signals_by_id.remove(&id).map(|signals| (id, signals)))
                .map(|(conflicting_entry, signals)| ConflictRecord {
                    id: ConflictId::generate(),
                    layer: layer.to_string(),
                    new_entry: entry.id,
                    conflicting_entry,
                    new_summary: entry.summary.clone(),
                    conflicting_summary: summaries.remove(&conflicting_entry).unwrap_or_default(),
                    signals,
                    detected_at: now,
                    status: ConflictStatus::Open,
                    resolved_by: None,
                    resolved_at: None,
                }),
        );
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(entries: Vec<Entry>) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for e in &entries {
            store.create(e).unwrap();
        }
        (dir, store)
    }

    fn verified(kind: EntryKind, summary: &str) -> Entry {
        let mut e = Entry::new("a", kind, summary);
        e.verify("review", None);
        e
    }

    #[test]
    fn shared_frame_flags_value_conflict() {
        let old = verified(EntryKind::Decision, "sessions use mysql");
        let (_d, store) = store_with(vec![old.clone()]);
        let new = Entry::new("a", EntryKind::Decision, "sessions use postgres");
        let found = detect(&store, "codex", &new, None).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].conflicting_entry, old.id);
        assert!(found[0].signals.contains(&ConflictSignal::SharedFrame));
    }

    #[test]
    fn negation_flags_reworded_conflict() {
        let old = verified(EntryKind::Decision, "deploy on fridays");
        let (_d, store) = store_with(vec![old.clone()]);
        let new = Entry::new("a", EntryKind::Decision, "never deploy on fridays");
        let found = detect(&store, "codex", &new, None).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0]
            .signals
            .contains(&ConflictSignal::NegationAsymmetry));
    }

    #[test]
    fn apostrophe_contractions_count_as_negation() {
        // The tokenizer splits "don't" into `don` (+ `t`, dropped): the
        // negation list must carry the contraction stems, not full words.
        let old = verified(EntryKind::Decision, "use postgres for storage");
        let (_d, store) = store_with(vec![old.clone()]);
        for claim in [
            "don't use postgres for storage",
            "we won't use postgres for storage",
            "sessions aren't on postgres for storage",
        ] {
            let new = Entry::new("a", EntryKind::Decision, claim);
            let found = detect(&store, "codex", &new, None).unwrap();
            assert_eq!(found.len(), 1, "{claim}");
            assert!(
                found[0]
                    .signals
                    .contains(&ConflictSignal::NegationAsymmetry),
                "{claim}"
            );
        }
    }

    #[test]
    fn numeric_mismatch_flags_changed_fact() {
        let old = verified(EntryKind::Observation, "api timeout is 30s");
        let (_d, store) = store_with(vec![old]);
        let new = Entry::new("a", EntryKind::Observation, "api timeout is 60s");
        let found = detect(&store, "codex", &new, None).unwrap();
        assert_eq!(found.len(), 1);
        assert!(found[0].signals.contains(&ConflictSignal::NumericMismatch));
    }

    #[test]
    fn orthogonal_claims_on_same_topic_do_not_flag() {
        // One shared topic word is not a claim frame; orthogonal facts must
        // not fill the queue.
        let old = verified(EntryKind::Decision, "checkout uses postgres");
        let (_d, store) = store_with(vec![old]);
        let new = Entry::new("a", EntryKind::Observation, "checkout p99 is 400ms");
        assert!(detect(&store, "codex", &new, None).unwrap().is_empty());
    }

    #[test]
    fn near_duplicate_does_not_flag() {
        let old = verified(EntryKind::Decision, "sessions use mysql everywhere");
        let (_d, store) = store_with(vec![old]);
        let mut new = Entry::new("a", EntryKind::Decision, "sessions use mysql everywhere");
        new.body = "same claim, restated".into();
        assert!(detect(&store, "codex", &new, None).unwrap().is_empty());
    }

    #[test]
    fn unverified_old_fact_never_flags() {
        let old = Entry::new("a", EntryKind::Decision, "sessions use mysql");
        let (_d, store) = store_with(vec![old]);
        let new = Entry::new("a", EntryKind::Decision, "sessions use postgres");
        assert!(detect(&store, "codex", &new, None).unwrap().is_empty());
    }

    #[test]
    fn supersession_and_evidence_targets_never_flag() {
        let old = verified(EntryKind::Decision, "sessions use mysql");
        let (_d, store) = store_with(vec![old.clone()]);
        let mut revision = Entry::new("a", EntryKind::Decision, "sessions use postgres");
        revision.revises = Some(old.id);
        assert!(detect(&store, "codex", &revision, None).unwrap().is_empty());

        let mut citing = Entry::new("a", EntryKind::Observation, "sessions use postgres now");
        citing.evidence.push(Evidence::Ref { id: old.id });
        assert!(detect(&store, "codex", &citing, None).unwrap().is_empty());
    }

    #[test]
    fn session_and_rollback_kinds_are_not_claims() {
        let old = verified(EntryKind::Decision, "sessions use mysql");
        let (_d, store) = store_with(vec![old]);
        let archive = Entry::new("a", EntryKind::Session, "sessions use postgres work done");
        assert!(detect(&store, "journal/a", &archive, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn record_roundtrips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Conflicts::open(dir.path().join(".conflicts"));
        assert!(queue.list().unwrap().is_empty());

        let rec = ConflictRecord {
            id: ConflictId::generate(),
            layer: "codex".into(),
            new_entry: EntryId::generate(),
            conflicting_entry: EntryId::generate(),
            new_summary: "sessions use postgres".into(),
            conflicting_summary: "sessions use mysql".into(),
            signals: vec![ConflictSignal::SharedFrame],
            detected_at: Utc::now(),
            status: ConflictStatus::Open,
            resolved_by: None,
            resolved_at: None,
        };
        queue.record(&rec).unwrap();
        assert_eq!(queue.require(rec.id).unwrap(), rec);
        assert_eq!(queue.list_open().unwrap().len(), 1);

        let mut resolved = rec.clone();
        resolved.status = ConflictStatus::Dismissed;
        resolved.resolved_by = Some("ops".into());
        resolved.resolved_at = Some(Utc::now());
        queue.update(&resolved).unwrap();
        assert!(queue.list_open().unwrap().is_empty());
        assert_eq!(
            queue.require(rec.id).unwrap().status,
            ConflictStatus::Dismissed
        );
    }

    #[test]
    fn corrupt_record_fails_strict_list_but_not_lenient_open() {
        let dir = tempfile::tempdir().unwrap();
        let queue = Conflicts::open(dir.path().join(".conflicts"));
        fs::create_dir_all(queue.dir()).unwrap();
        fs::write(
            queue.dir().join("01ARZ3NDEKTSV4RRFFQ69G5FAV.md"),
            "not yaml at all",
        )
        .unwrap();

        // The strict read reports corruption; the lenient read skips it.
        assert!(queue.list().is_err());
        assert!(queue.open_lenient().unwrap().is_empty());
    }
}
