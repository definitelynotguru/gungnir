//! Recall: BM25-lite scoring over a stemmed token vocabulary, ordered by
//! verification bucket first so verified facts outrank hearsay and
//! contradicted facts sink. Rolled-back entries are hidden by default.
//!
//! Temporal modes:
//! - `as_of` evaluates candidate entries as they existed at an instant,
//!   derived from verification-log timestamps. No schema beyond what is
//!   already written.
//! - `current_only` resolves revises chains to their heads and drops
//!   contradicted facts, answering "what do we believe now".
//!
//! Ranking formula (same in keyword and hybrid mode, documented in
//! docs/DESIGN.md):
//!
//!   final = relevance × trust × recency
//!
//! - `relevance` is the BM25-lite score for keyword recall, or the RRF
//!   fusion score for hybrid recall.
//! - `trust` is the verification bucket as a multiplier: verified 1.0,
//!   unverified 0.75, contradicted 0.35. Rolled-back facts never rank; they
//!   are filtered before scoring.
//! - `recency` is an exponential decay in the entry's age measured at the
//!   query instant (`as_of`, or now), half-life 90 days, floored at 0.25 so
//!   old-but-true facts still surface.
//!
//! Ordering sorts the verification bucket lexicographically first, then the
//! final score: a contradicted entry can never outrank a verified one on
//! the same topic, no matter how relevant it is.
//!
//! Standing constraints: a deterministic cue tagger marks rules and
//! preferences, and task-shaped queries union a capped, appended slice of
//! verified entries into spare result slots. The lane is derived from the
//! canonical files on every query; it writes nothing, verifies nothing, and
//! fact-shaped (including temporal) queries never receive it.
//!
//! Entity and session expansion builds a derived adjacency map on each query
//! from the temporal pool. Keys come from normalized `entity_mentions()` and
//! `session_id`, with separate prefixes so the two cannot collide. Live
//! verified lexical or vector hits seed one hop to verified neighbors that
//! share a key. At most `EXPANSION_LANE_CAP` neighbors append after the
//! ordered hits and the standing lane, under `limit`, with score 0.0 and no
//! coverage change. The map is read-only and never persisted.
//!
//! Slot resolution: an explicit `revises` chain is one slot, and its head is
//! the member no live `revises` link points at. `current_only` hides the
//! link targets; a plain present-time query instead ranks those verified
//! tails below current knowledge, so a stale wording cannot beat the current
//! fact on BM25 score alone. Both views share the link-target set from
//! `temporal_pool`, so they never disagree. The chain is derived from
//! canonical files on every query; nothing is written, no verification
//! state changes, and `as_of` reads keep historical semantics.
//!
//! Answer binding: `require_evidence` adds a deterministic gate over the
//! closed-class parts of a question. These are curated role/title modifiers,
//! digit-bearing tokens, and negation presence. It drops every hit whose
//! summary and body do not carry them. It answers "does the returned
//! evidence bind the question", which `require_verified` cannot: a verified
//! junior engineer is not an answer to a question about a senior one. The
//! gate never reorders, rescans, or re-scores. It filters ordered hits in
//! `finish` after the provenance filter and excludes the standing and
//! expansion lanes so off-topic entries cannot satisfy a constraint. Scores
//! and coverage stay unchanged. Open-class terms are left to ranking on
//! purpose, so paraphrase still matches. No model judges eligibility.

use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

use crate::entry::{Entry, EntryKind, RecordStatus, VerificationState};
use crate::id::EntryId;
use crate::store::Skipped;
use crate::{Result, Store};

mod dates;

#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub limit: usize,
    /// Resolve relative dates in the question at this explicit instant.
    /// None leaves relative query dates unresolved.
    pub question_time: Option<DateTime<Utc>>,
    /// Evaluate candidates as they existed at an instant instead of now.
    pub as_of: Option<DateTime<Utc>>,
    /// Keep only revises-chain heads and drop contradicted facts.
    pub current_only: bool,
    /// Return only verified hits. Coverage still counts the whole matched
    /// pool, so a caller can abstain explicitly when verified is zero.
    pub require_verified: bool,
    /// Return only hits whose evidence binds the closed-class constraints
    /// the question states: curated role/title modifiers, digit-bearing
    /// tokens, and negation in yes/no questions. Provenance is untouched. This asks
    /// whether a hit *answers* the question, not whether it is verified,
    /// and open-class wording is left to ranking so paraphrase still
    /// matches. Coverage and scores are unchanged.
    pub require_evidence: bool,
}

impl Query {
    pub fn new(text: impl Into<String>, limit: usize) -> Self {
        Self {
            text: text.into(),
            limit,
            question_time: None,
            as_of: None,
            current_only: false,
            require_verified: false,
            require_evidence: false,
        }
    }

    pub fn question_time(mut self, at: DateTime<Utc>) -> Self {
        self.question_time = Some(at);
        self
    }

    pub fn as_of(mut self, at: DateTime<Utc>) -> Self {
        self.as_of = Some(at);
        self
    }

    pub fn current(mut self) -> Self {
        self.current_only = true;
        self
    }

    pub fn require_verified(mut self) -> Self {
        self.require_verified = true;
        self
    }

    pub fn require_evidence(mut self) -> Self {
        self.require_evidence = true;
        self
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Hit {
    pub entry: Entry,
    /// Verification state evaluated at the query's `as_of` — the state the
    /// hit was *returned under*. `entry.verification` still carries now, so
    /// machine readers and briefing labels must use this, not the entry.
    pub state: StateAt,
    pub score: f64,
}

/// Topic-scoped counts behind a result set. Powers the abstention signal:
/// "no verified knowledge" is a claim about coverage, not just empty output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Coverage {
    pub verified: usize,
    pub unverified: usize,
    pub contradicted: usize,
    /// Chain tails excluded by `current_only`.
    pub hidden_superseded: usize,
    pub hidden_rolled_back: usize,
    /// Every entry in scope (after the `as_of` filter), matched or not —
    /// rolled-back entries included. Distinguishes "the layer is empty"
    /// from "entries exist but none matched the topic".
    pub total: usize,
}

impl Coverage {
    pub fn total_visible(&self) -> usize {
        self.verified + self.unverified
    }

    /// Total entries the topic matched, across every visible state.
    pub fn total_matched(&self) -> usize {
        self.verified + self.unverified + self.contradicted
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    pub coverage: Coverage,
    /// ULID-named files that failed to parse and were skipped.
    pub skipped: Vec<Skipped>,
    #[serde(skip)]
    pub evidence_filtered: usize,
}

impl SearchOutcome {
    /// Whether this result should be reported as an explicit refusal. One
    /// decision for every surface, so CLI prose, `--json`, and MCP cannot
    /// disagree about what counts as abstaining.
    ///
    /// Provenance abstention fires when `require_verified` found no verified
    /// knowledge on the topic. This is a claim about coverage, not output
    /// size. Answer binding fires when `require_evidence` leaves no returned
    /// evidence at a nonzero limit. An empty page at `limit: 0` is the
    /// caller's own choice, so it never counts as an answer-binding miss.
    pub fn abstained(&self, query: &Query) -> bool {
        (query.require_verified && self.coverage.verified == 0)
            || (query.require_evidence && query.limit > 0 && self.hits.is_empty())
    }
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "in",
    "is", "it", "its", "of", "on", "or", "that", "the", "this", "to", "was", "were", "will",
    "with",
];

/// Lowercase alphanumeric tokens, stopwords stripped and stemmed. Queries
/// and entries pass through the same pipeline, so "migrating" matches
/// "migration" and "retries" matches "retried".
pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(t))
        .map(stem)
        .collect()
}

fn ends_consonant(w: &str) -> bool {
    w.chars()
        .last()
        .is_some_and(|c| c.is_ascii_alphabetic() && !"aeiou".contains(c))
}

/// A compact English stemmer, Porter-flavored but deliberately smaller:
/// enough morphology to collapse the inflections an agent actually
/// produces ("migrating"/"migration"/"migrated"), nothing more. Applied on
/// both sides of the comparison, so an occasional over-stem costs nothing.
fn stem(word: &str) -> String {
    let mut w = word.to_string();
    // Pass 1 strips one plural marker ("connections" → "connection"); it
    // runs before the derivational endings so "postings" loses "s", not
    // "ing". The bare "s" strip skips terminal "ss"/"us"/"is"/"as" — those
    // are singular endings, not plurals — so "status"/"statuses" and
    // "alias"/"aliases" meet at the singular. An "es" strip counts as the
    // plural marker, so it never triggers a second strip: the 's' left
    // behind ("responses" → "respons") belongs to the base.
    let mut stripped = false;
    for (suffix, repl) in [("sses", "ss"), ("ies", "i"), ("es", "")] {
        if let Some(base) = w.strip_suffix(suffix) {
            if base.len() >= 2 {
                w = base.to_string() + repl;
                stripped = true;
                break;
            }
        }
    }
    if !stripped
        && w.ends_with('s')
        && !["ss", "us", "is", "as"].iter().any(|e| w.ends_with(e))
        && w.len() > 3
    {
        w.pop();
    }
    // Pass 2 strips derivational endings ("connection" → "connect").
    for (suffix, repl) in [
        ("ational", "ate"),
        ("tional", "tion"),
        ("ing", ""),
        ("ed", ""),
        ("ation", "ate"),
        ("tion", "t"),
        ("ly", ""),
    ] {
        if let Some(base) = w.strip_suffix(suffix) {
            if base.len() >= 2 {
                w = base.to_string() + repl;
                break;
            }
        }
    }
    // Doubled consonant left by "ing"/"ed" stripping: "dropped" → "drop".
    let mut chars: Vec<char> = w.chars().collect();
    if chars.len() > 3 {
        let n = chars.len();
        if chars[n - 1] == chars[n - 2] && !"lsz".contains(chars[n - 1]) {
            chars.pop();
            w = chars.iter().collect();
        }
    }
    // Silent trailing e: "migrate" → "migrat", matching the "ation"→"ate"
    // rule above. No consonant precondition: vowel+e words keep the family
    // together too ("value" and "values" both land on "valu", "issue" and
    // "issues" on "issu"). The length floor only protects 1–2 letter stems.
    if w.len() > 2 && w.ends_with('e') {
        w.pop();
    }
    // Terminal consonant-y: "retry" → "retri" to meet "retries" → "retri".
    // The floor matches the ies rule's own length requirement, so short
    // bases keep up too: "try" and "tries" both land on "tri".
    if w.len() > 2 && w.ends_with('y') && ends_consonant(&w[..w.len() - 1]) {
        w.pop();
        w.push('i');
    }
    w
}

/// Rank bucket at the present moment: higher sorts first.
pub fn bucket(entry: &Entry) -> u8 {
    match entry.verification {
        VerificationState::Verified => 3,
        VerificationState::Unverified => 2,
        VerificationState::Contradicted { .. } => 1,
        VerificationState::RolledBack => 0,
    }
}

/// Verification state evaluated from the append-only log at `as_of`.
/// The log is the historical record; the frontmatter field only holds now.
/// Public because audit snapshots expose it as the state of an entry at a
/// point in time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StateAt {
    Verified,
    Unverified,
    Contradicted,
    RolledBack,
}

pub(crate) fn state_at(entry: &Entry, as_of: Option<DateTime<Utc>>) -> StateAt {
    let Some(cutoff) = as_of else {
        return match entry.verification {
            VerificationState::Verified => StateAt::Verified,
            VerificationState::Contradicted { .. } => StateAt::Contradicted,
            VerificationState::RolledBack => StateAt::RolledBack,
            VerificationState::Unverified => StateAt::Unverified,
        };
    };
    entry
        .verification_log
        .iter()
        .filter(|r| r.timestamp <= cutoff)
        .max_by_key(|r| r.timestamp)
        .map(|r| match r.status {
            RecordStatus::Verified => StateAt::Verified,
            RecordStatus::Contradicted => StateAt::Contradicted,
            RecordStatus::RolledBack => StateAt::RolledBack,
            RecordStatus::Unverified => StateAt::Unverified,
        })
        .unwrap_or(StateAt::Unverified)
}

fn bucket_at(entry: &Entry, as_of: Option<DateTime<Utc>>) -> u8 {
    match state_at(entry, as_of) {
        StateAt::Verified => 3,
        StateAt::Unverified => 2,
        StateAt::Contradicted => 1,
        StateAt::RolledBack => 0,
    }
}

fn trust_weight(state: StateAt) -> f64 {
    match state {
        StateAt::Verified => 1.0,
        StateAt::Unverified => 0.75,
        StateAt::Contradicted => 0.35,
        StateAt::RolledBack => 0.0,
    }
}

/// Exponential recency decay, half-life 90 days, floored so a
/// still-true old fact keeps a floor of rank instead of decaying to zero.
pub(crate) fn recency_weight(entry: &Entry, now: DateTime<Utc>) -> f64 {
    const HALF_LIFE_SECS: f64 = 90.0 * 86_400.0;
    const FLOOR: f64 = 0.25;
    let age = (now - entry.timestamp).num_seconds().max(0) as f64;
    let decay = 0.5f64.powf(age / HALF_LIFE_SECS);
    decay.max(FLOOR)
}

/// One entry inside the temporal candidate pool with its evaluated state.
pub(crate) struct Candidate {
    pub entry: Entry,
    pub state: StateAt,
}

/// The candidate pool after `as_of` filtering, plus the set of ids that a
/// live `revises` link points at (superseded tails). Shared by keyword and
/// hybrid search, and by both current views: `current_only` hides those ids,
/// while a plain present-time query only demotes them. One definition of a
/// slot head, so the two views can never disagree about which member is
/// current.
///
/// `revised` collects revises targets from every as-of-valid live entry,
/// not just topic-matched ones: otherwise a chain A<-B<-C where B is
/// off-topic leaves A visible next to C.
pub(crate) fn temporal_pool(
    store: &Store,
    query: &Query,
) -> Result<(Vec<Candidate>, HashSet<EntryId>, Vec<Skipped>)> {
    let mut pool = Vec::new();
    let mut revised = HashSet::new();
    let scan = store.scan()?;
    for entry in scan.entries {
        if let Some(cutoff) = query.as_of {
            if entry.timestamp > cutoff {
                continue;
            }
        }
        let state = state_at(&entry, query.as_of);
        // A rolled-back head must not hide its predecessor. A rollback
        // record's `revises` marks the ancestor it restores, so it is not a
        // revision. Both current views use this same `revised` set.
        if state != StateAt::RolledBack && entry.kind != EntryKind::Rollback {
            if let Some(prev) = entry.revises {
                revised.insert(prev);
            }
        }
        pool.push(Candidate { entry, state });
    }
    Ok((pool, revised, scan.skipped))
}

/// Verified slot tails that a plain present-time query demotes below current
/// knowledge. Slot identity is the `revised` set from `temporal_pool`: the
/// ids that live `revises` links point at. `current_only` hides that same
/// set, so both current views share one head definition.
///
/// An unverified proposal is never demoted: it keeps its own trust bucket, so
/// slot resolution cannot call hearsay stale by a read-path judgment. `as_of`
/// views skip this entirely, keeping historical ordering.
fn stale_slot_members(pool: &[Candidate], revised: &HashSet<EntryId>) -> HashSet<EntryId> {
    pool.iter()
        .filter(|c| {
            c.state == StateAt::Verified
                && c.entry.kind != EntryKind::Rollback
                && revised.contains(&c.entry.id)
        })
        .map(|c| c.entry.id)
        .collect()
}

/// Cues that mark an entry as a standing constraint: a durable rule or
/// preference meant to shape future work. Matching is deterministic and
/// read-only — no model decides what is standing, and a cue never verifies
/// the entry. Written as normalized whole-word sequences (lowercase,
/// punctuation split), so "don't" is "don t" and "I'd prefer" is "i d prefer".
const STANDING_CUES: &[&str] = &[
    "always",
    "never",
    "from now on",
    "i prefer",
    "we prefer",
    "prefer to",
    "i d prefer",
    "don t",
    "do not",
    "must not",
    "must always",
    "should not",
    "should always",
    "make sure",
    "remember to",
    "whenever",
];

/// Max standing constraints a task-shaped recall may append. The lane fills
/// spare slots below `limit`; it never evicts or reorders a lexical hit.
const STANDING_LANE_CAP: usize = 4;

/// Max entity or session neighbors appended by expansion. This is a second,
/// independent budget: it does not change `STANDING_LANE_CAP`.
pub(crate) const EXPANSION_LANE_CAP: usize = 6;

/// Fact-shaped queries ask about stored knowledge. A generic standing rule
/// in a temporal or knowledge-update top-k is a false hit, so these cues
/// take precedence over task cues and keep the lane out entirely.
const FACT_CUES: &[&str] = &[
    "what",
    "who",
    "where",
    "which",
    "why",
    "whose",
    "how many",
    "how much",
    "how long",
    "how old",
    "how often",
    "how did",
    "how was",
    "how were",
    "when",
    "ago",
    "as of",
    "before",
    "after",
    "originally",
    "previously",
    "used to",
    "did",
    "was",
    "were",
    "happened",
    "happen",
    "current",
    "currently",
    "latest",
    "still",
    "no longer",
    "anymore",
    "changed",
    "moved to",
    "instead of",
    "updated",
    "superseded",
];

/// Task-shaped queries ask the agent to produce or act. Only a query with
/// one of these and no fact cue gets the lane; every other query stays
/// fact-shaped. Content-shape markers, not topic words.
const TASK_CUES: &[&str] = &[
    "show me",
    "show us",
    "give me",
    "give us",
    "give an",
    "write",
    "build",
    "implement",
    "implementation",
    "create",
    "generate",
    "make a",
    "make me",
    "make an",
    "draft",
    "refactor",
    "design",
    "add a",
    "add an",
    "add the",
    "set up",
    "scaffold",
    "can you",
    "could you",
    "would you",
    "please",
    "help me",
    "let s",
    "i need",
    "i want",
    "i d like",
    "i would like",
    "how do i",
    "how should i",
    "how can i",
    "how to",
    "how would i",
    "produce",
    "outline",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryShape {
    Task,
    Fact,
}

/// Lowercase whole-word sequence, punctuation split. Cues match whole words,
/// so "never" does not fire on "nevertheless" and stemming cannot mangle a
/// phrase the way `tokenize` would.
fn normalized(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when a cue phrase appears as a contiguous whole-word sequence.
fn has_cue(words: &[String], cues: &[&str]) -> bool {
    cues.iter().any(|cue| {
        let phrase: Vec<&str> = cue.split_whitespace().collect();
        !phrase.is_empty()
            && phrase.len() <= words.len()
            && words.windows(phrase.len()).any(|w| {
                w.iter()
                    .zip(phrase.iter().copied())
                    .all(|(a, b)| a.as_str() == b)
            })
    })
}

/// Single-word task cues also match common inflections by stemming the
/// word ("building" meets "build", "writing" meets "write"). Multi-word cues
/// stay exact, so stemming cannot invent a phrase or loosen the fact and
/// standing cue matchers, which remain whole-word.
fn has_task_cue(words: &[String]) -> bool {
    TASK_CUES.iter().any(|cue| {
        let phrase: Vec<&str> = cue.split_whitespace().collect();
        if phrase.len() == 1 {
            let key = stem(phrase[0]);
            words.iter().any(|w| stem(w) == key)
        } else {
            phrase.len() <= words.len()
                && words.windows(phrase.len()).any(|w| {
                    w.iter()
                        .zip(phrase.iter().copied())
                        .all(|(a, b)| a.as_str() == b)
                })
        }
    })
}

/// Route by query shape, not topic words. `require_verified` is a strict
/// provenance query and `require_evidence` is a strict answer query, so
/// standing constraints stay out of both. Temporal and current-view queries
/// are fact-shaped outright, independent of their wording. Fact cues win
/// over task cues. An unrecognized query defaults to fact-shaped, the
/// conservative direction that keeps generic rules out of fact top-k.
fn query_shape(query: &Query) -> QueryShape {
    if query.require_verified
        || query.require_evidence
        || query.as_of.is_some()
        || query.current_only
    {
        return QueryShape::Fact;
    }
    let words = normalized(&query.text);
    if has_cue(&words, FACT_CUES) {
        return QueryShape::Fact;
    }
    if has_task_cue(&words) {
        return QueryShape::Task;
    }
    QueryShape::Fact
}

/// Deterministic cue tagger. The lane is derived from the canonical entries
/// on every query; files stay the store of truth and nothing is persisted.
/// Summary and body are checked separately, so a multi-word cue cannot be
/// assembled across the field boundary ("...from now" + "on..." is not
/// "from now on").
fn is_standing_constraint(entry: &Entry) -> bool {
    has_cue(&normalized(&entry.summary), STANDING_CUES)
        || has_cue(&normalized(&entry.body), STANDING_CUES)
}

/// The capped standing-constraint slice for a task-shaped query. Only
/// verified entries qualify: unverified and contradicted journal content
/// must not steer a task briefing (EXTRACTION-THREAT-MODEL.md). Drawn from
/// the same visible candidate pool as the lexical hits, so `as_of`,
/// `current_only`, rollback, and strict query flags apply through the normal
/// live-entry rules. Rebuilt each time (a derived access view):
/// newest-first by the entry's own timestamp, with the EntryId as a
/// deterministic tie-break when timestamps are equal. The slice stays
/// stable when the cap binds.
fn standing_lane(
    pool: &[Candidate],
    revised: &HashSet<EntryId>,
    query: &Query,
    lexical_ids: &HashSet<EntryId>,
) -> Vec<(Entry, StateAt, f64)> {
    if query_shape(query) != QueryShape::Task {
        return Vec::new();
    }
    let mut lane: Vec<(Entry, StateAt, f64)> = visible_indices(pool, revised, query)
        .into_iter()
        .map(|i| (pool[i].entry.clone(), pool[i].state))
        .filter(|(e, st)| {
            *st == StateAt::Verified && is_standing_constraint(e) && !lexical_ids.contains(&e.id)
        })
        .map(|(e, st)| (e, st, 0.0))
        .collect();
    lane.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp).then(b.0.id.cmp(&a.0.id)));
    lane.truncate(STANDING_LANE_CAP);
    lane
}

/// The capped entity and session expansion slice. One derived adjacency map
/// per query holds the index of every pool entry under each normalized
/// `entity_mentions()` string and `session_id`; prefixes keep the two key
/// namespaces separate. Every live verified positive-relevance hit in
/// `scored` seeds one hop to neighbors sharing a key, so the lane follows
/// topical relevance rather than the standing-constraint cues. Only verified,
/// visible neighbors qualify, and `existing_ids` keeps seeds and standing-lane
/// hits out. The slice is a read-only view over the same temporal pool, so
/// `as_of`, `current_only`, rollback, and latest-slot resolution stay in force.
fn expansion_lane(
    pool: &[Candidate],
    revised: &HashSet<EntryId>,
    query: &Query,
    scored: &[(Entry, StateAt, f64)],
    existing_ids: &HashSet<EntryId>,
) -> Vec<(Entry, StateAt, f64)> {
    if query.require_verified || query.require_evidence {
        return Vec::new();
    }

    let mut adjacency: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, c) in pool.iter().enumerate() {
        for name in c.entry.entity_mentions() {
            adjacency
                .entry(format!("entity:{name}"))
                .or_default()
                .push(i);
        }
        if let Some(session) = c.entry.session_id.as_deref() {
            adjacency
                .entry(format!("session:{session}"))
                .or_default()
                .push(i);
        }
    }

    let visible: HashSet<usize> = visible_indices(pool, revised, query).into_iter().collect();
    let mut lane: Vec<(Entry, StateAt, f64)> = Vec::new();
    let mut chosen: HashSet<usize> = HashSet::new();
    for (seed, state, relevance) in scored {
        if *state != StateAt::Verified || *relevance <= 0.0 {
            continue;
        }
        let keys = seed
            .entity_mentions()
            .into_iter()
            .map(|name| format!("entity:{name}"))
            .chain(seed.session_id.as_deref().map(|s| format!("session:{s}")));
        for key in keys {
            let Some(indices) = adjacency.get(&key) else {
                continue;
            };
            for &i in indices {
                let c = &pool[i];
                if !visible.contains(&i)
                    || c.state != StateAt::Verified
                    || existing_ids.contains(&c.entry.id)
                    || !chosen.insert(i)
                {
                    continue;
                }
                lane.push((c.entry.clone(), c.state, 0.0));
            }
        }
    }

    lane.sort_by(|a, b| b.0.timestamp.cmp(&a.0.timestamp).then(b.0.id.cmp(&a.0.id)));
    lane.truncate(EXPANSION_LANE_CAP);
    lane
}

const TITLE_MODIFIERS: &[&str] = &[
    "senior",
    "junior",
    "manager",
    "lead",
    "principal",
    "staff",
    "director",
    "chief",
    "intern",
    "associate",
    "assistant",
    "vp",
    "president",
    "distinguished",
];

const TITLE_FRAMES: &[&str] = &[
    "engineer",
    "engineering",
    "developer",
    "programmer",
    "designer",
    "architect",
    "scientist",
    "analyst",
    "consultant",
    "officer",
    "administrator",
    "specialist",
    "coordinator",
    "executive",
    "president",
];

const NEGATION_WORDS: &[&str] = &[
    "no", "not", "never", "none", "nobody", "nothing", "neither", "nor", "without", "cannot",
    "cant", "dont", "wont", "isnt", "arent", "wasnt", "werent", "didnt", "doesnt", "couldnt",
    "shouldnt", "mustnt", "neednt", "hasnt", "hadnt", "aint", "shant",
];

const NEGATION_CONTRACTIONS: &[&str] = &[
    "don", "won", "isn", "aren", "wasn", "weren", "didn", "doesn", "couldn", "shouldn", "mustn",
    "needn", "hasn", "hadn", "ain", "shan",
];

const YES_NO_STARTERS: &[&str] = &[
    "am", "are", "is", "was", "were", "do", "does", "did", "can", "could", "will", "would",
    "shall", "should", "may", "might", "must", "have", "has", "had",
];

fn negates(tokens: &[String]) -> bool {
    tokens.iter().enumerate().any(|(i, token)| {
        NEGATION_WORDS.iter().any(|n| n == token)
            && !(token == "no" && tokens.get(i + 1).is_some_and(|next| next == "longer"))
    }) || tokens
        .windows(2)
        .any(|pair| pair[1] == "t" && NEGATION_CONTRACTIONS.iter().any(|n| *n == pair[0]))
}

fn answer_negated(tokens: &[String]) -> bool {
    let yes_no = tokens.first().is_some_and(|first| {
        YES_NO_STARTERS.contains(&first.as_str()) || NEGATION_CONTRACTIONS.contains(&first.as_str())
    });
    yes_no
        && tokens.iter().enumerate().any(|(i, token)| {
            i > 2
                && NEGATION_WORDS.iter().any(|n| n == token)
                && !(token == "no" && tokens.get(i + 1).is_some_and(|next| next == "longer"))
        })
        || (yes_no
            && tokens.windows(2).enumerate().any(|(i, pair)| {
                i > 2 && pair[1] == "t" && NEGATION_CONTRACTIONS.iter().any(|n| *n == pair[0])
            }))
}

fn is_title_frame(token: &str) -> bool {
    TITLE_FRAMES.contains(&token)
        || token == "manager"
        || token
            .strip_suffix('s')
            .is_some_and(|singular| TITLE_FRAMES.contains(&singular))
}

fn modifier_form(token: &str) -> &str {
    token.strip_suffix('s').unwrap_or(token)
}

fn is_role_modifier(tokens: &[String], index: usize, modifier: &str) -> bool {
    const QUALIFIERS: &[&str] = &[
        "software",
        "hardware",
        "system",
        "systems",
        "technology",
        "technical",
        "platform",
        "product",
        "security",
        "data",
        "machine",
        "learning",
        "site",
        "reliability",
        "devops",
        "design",
        "frontend",
        "backend",
        "test",
    ];

    let token = &tokens[index];
    if token != modifier
        && modifier_form(token) == modifier
        && index > 0
        && matches!(tokens[index - 1].as_str(), "which" | "what" | "the")
    {
        return true;
    }
    if token == "president" {
        return true;
    }
    if token == "manager"
        && (index > 0
            && (is_title_frame(&tokens[index - 1])
                || TITLE_MODIFIERS.contains(&tokens[index - 1].as_str()))
            || tokens.get(index + 1).is_some_and(|next| next == "of")
                && tokens
                    .get(index + 2)
                    .is_some_and(|next| is_title_frame(next)))
    {
        return true;
    }
    tokens
        .get(index + 1)
        .is_some_and(|next| is_title_frame(next))
        || (tokens
            .get(index + 1)
            .is_some_and(|next| QUALIFIERS.contains(&next.as_str()))
            && tokens
                .get(index + 2)
                .is_some_and(|next| is_title_frame(next)))
}

fn binds_title_modifier(tokens: &[String], modifier: &str) -> bool {
    tokens.iter().enumerate().any(|(index, token)| {
        modifier_form(token) == modifier
            && !(token != modifier
                && ["lead", "staff", "associate"].contains(&modifier)
                && index > 0
                && is_title_frame(&tokens[index - 1]))
    })
}

fn canonical_number(token: &str) -> String {
    let digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|suffix| token.strip_suffix(suffix))
        .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(token);
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return token.to_string();
    }
    let trimmed = digits.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_comma_groups(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        let first_end = i;
        let mut end = first_end;
        if first_end - start <= 3 {
            while end < chars.len()
                && chars[end] == ','
                && end + 3 < chars.len()
                && chars[end + 1..end + 4].iter().all(|c| c.is_ascii_digit())
                && chars.get(end + 4).is_none_or(|c| !c.is_ascii_digit())
            {
                end += 4;
            }
        }
        if end > first_end {
            for c in &chars[start..first_end] {
                out.push(*c);
            }
            let mut group = first_end;
            while group < end {
                group += 1;
                for c in &chars[group..group + 3] {
                    out.push(*c);
                }
                group += 3;
            }
        } else {
            for c in &chars[start..first_end] {
                out.push(*c);
            }
        }
    }
    out
}

fn numeric_tokens(tokens: &[String], question_framing: bool) -> HashSet<String> {
    const TIME_UNITS: &[&str] = &[
        "second", "seconds", "minute", "minutes", "hour", "hours", "day", "days", "week", "weeks",
        "month", "months", "year", "years",
    ];

    let mut numbers = HashSet::new();
    let mut i = 0;
    while i < tokens.len() {
        let token = &tokens[i];
        if !token.chars().any(|c| c.is_ascii_digit()) {
            i += 1;
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) {
            let relative_time = tokens
                .get(i + 1)
                .is_some_and(|unit| TIME_UNITS.contains(&unit.as_str()))
                && tokens.get(i + 2).is_some_and(|word| word == "ago");
            let list_size = i > 0 && tokens[i - 1] == "top";
            if question_framing && (relative_time || list_size) {
                i += 1;
                continue;
            }
            let compact_minute = tokens.get(i + 1).and_then(|next| {
                ["am", "pm"].iter().find_map(|suffix| {
                    next.strip_suffix(suffix)
                        .filter(|minute| {
                            minute.len() == 2 && minute.chars().all(|c| c.is_ascii_digit())
                        })
                        .map(|minute| (minute, suffix))
                })
            });
            if let Some((minute, suffix)) = compact_minute {
                numbers.insert(format!("{}:{}{}", canonical_number(token), minute, suffix));
                i += 2;
                continue;
            }
            if tokens
                .get(i + 1)
                .is_some_and(|suffix| suffix == "am" || suffix == "pm")
            {
                numbers.insert(format!("{}:00{}", canonical_number(token), tokens[i + 1]));
                i += 2;
                continue;
            }
            if tokens.get(i + 1).is_some_and(|minute| {
                minute.len() == 2 && minute.chars().all(|c| c.is_ascii_digit())
            }) && tokens
                .get(i + 2)
                .is_some_and(|suffix| suffix == "am" || suffix == "pm")
            {
                numbers.insert(format!(
                    "{}:{}{}",
                    canonical_number(token),
                    tokens[i + 1],
                    tokens[i + 2]
                ));
                i += 3;
                continue;
            }
            numbers.insert(canonical_number(token));
            i += 1;
            continue;
        }
        let meridiem = ["am", "pm"]
            .iter()
            .find_map(|suffix| token.strip_suffix(suffix).map(|hour| (hour, suffix)));
        if let Some((hour, suffix)) = meridiem {
            if hour.chars().all(|c| c.is_ascii_digit()) {
                numbers.insert(format!("{}:00{}", canonical_number(hour), suffix));
                i += 1;
                continue;
            }
        }
        numbers.insert(canonical_number(token));
        i += 1;
    }
    numbers
}

#[derive(Debug, Default)]
struct ConstraintView {
    tokens: Vec<String>,
    numbers: HashSet<String>,
    negated: bool,
}

impl ConstraintView {
    fn of(parts: &[&str], question_framing: bool) -> Self {
        let tokens: Vec<String> = parts.iter().flat_map(|part| normalized(part)).collect();
        let numeric_text: Vec<String> = parts
            .iter()
            .flat_map(|part| normalized(&normalize_comma_groups(part)))
            .collect();
        Self {
            numbers: numeric_tokens(&numeric_text, question_framing),
            negated: negates(&tokens),
            tokens,
        }
    }
}

#[derive(Debug, Default)]
struct AnswerConstraints {
    title_modifiers: Vec<&'static str>,
    numbers: HashSet<String>,
    negated: bool,
}

impl AnswerConstraints {
    fn parse(text: &str, date_selected: bool) -> Self {
        let masked = date_selected.then(|| dates::mask_absolute_dates(text));
        let view = ConstraintView::of(&[masked.as_deref().unwrap_or(text)], true);
        Self {
            title_modifiers: TITLE_MODIFIERS
                .iter()
                .copied()
                .filter(|modifier| {
                    view.tokens.iter().enumerate().any(|(i, token)| {
                        modifier_form(token) == *modifier
                            && is_role_modifier(&view.tokens, i, modifier)
                    })
                })
                .collect(),
            numbers: view.numbers,
            negated: answer_negated(&view.tokens),
        }
    }

    fn permits(&self, entry: &Entry) -> bool {
        let view = ConstraintView::of(&[&entry.summary, &entry.body], false);
        self.title_modifiers
            .iter()
            .all(|modifier| binds_title_modifier(&view.tokens, modifier))
            && self.numbers.iter().all(|n| view.numbers.contains(n))
            && (!self.negated || view.negated)
    }
}

/// Shared tail of every search: coverage accounting, trust×recency
/// weighting, bucket-first ordering, truncation. Standing and expansion
/// slices append after the lexical hits under their own caps; coverage
/// counts only the topic-matched lexical pool.
///
/// `matched` holds topic-matched candidates with their raw relevance
/// (BM25-lite or RRF). Coverage counts run over that pool before
/// truncation, so callers can say "no verified knowledge covers this task"
/// even when hits exist.
///
/// `require_evidence` filters ordered hits after the provenance filter and
/// before truncation. Strict answer queries exclude both derived lanes, so
/// only topical evidence can satisfy the question. Scores and coverage stay
/// unchanged.
pub(crate) fn finish(
    mut matched: Vec<(Entry, StateAt, f64)>,
    pool: &[Candidate],
    revised: &HashSet<EntryId>,
    query: &Query,
    total: usize,
    skipped: Vec<Skipped>,
) -> SearchOutcome {
    let date_selection = dates::select_date_matches(pool, query);
    let date_selected = date_selection.is_some();
    let mut exact_date_ids = HashSet::new();
    if let Some(dates::DateSelection {
        matched_ids,
        exact_ids,
    }) = date_selection
    {
        exact_date_ids = exact_ids;
        let raw_by_id: HashMap<EntryId, f64> = matched
            .into_iter()
            .map(|(entry, _, relevance)| (entry.id, relevance))
            .collect();
        matched = pool
            .iter()
            .filter(|candidate| matched_ids.contains(&candidate.entry.id))
            .map(|candidate| {
                let raw = raw_by_id.get(&candidate.entry.id).copied().unwrap_or(0.0);
                (candidate.entry.clone(), candidate.state, 1.0 + raw)
            })
            .collect();
    }

    let mut coverage = Coverage {
        total,
        ..Coverage::default()
    };

    matched.retain(|(e, st, _)| {
        if *st == StateAt::RolledBack {
            coverage.hidden_rolled_back += 1;
            return false;
        }
        if query.current_only && revised.contains(&e.id) {
            coverage.hidden_superseded += 1;
            return false;
        }
        if query.current_only && *st == StateAt::Contradicted {
            coverage.contradicted += 1;
            return false;
        }
        true
    });

    for (_, st, _) in &matched {
        match st {
            StateAt::Verified => coverage.verified += 1,
            StateAt::Unverified => coverage.unverified += 1,
            StateAt::Contradicted => coverage.contradicted += 1,
            StateAt::RolledBack => unreachable!("rolled back filtered above"),
        }
    }

    // Plain present-time queries demote the same link-target set that
    // `current_only` hides: the verified tails rank below current knowledge
    // on a tie-break inside the trust bucket. One shared head definition for
    // both views; `as_of` reads keep their historical ordering.
    let stale_slots = if query.as_of.is_none() && !query.current_only {
        stale_slot_members(pool, revised)
    } else {
        HashSet::new()
    };
    let slot_stale = |e: &Entry| u8::from(stale_slots.contains(&e.id));
    let date_exact = |e: &Entry| u8::from(exact_date_ids.contains(&e.id));

    let now = query.as_of.unwrap_or_else(Utc::now);
    let mut scored: Vec<(Entry, StateAt, f64)> = matched
        .into_iter()
        .map(|(e, st, rel)| {
            let final_score = rel * trust_weight(st) * recency_weight(&e, now);
            (e, st, final_score)
        })
        .collect();

    scored.sort_by(|a, b| {
        // Verification bucket stays first: a contradicted head must never
        // outrank a verified stale tail. Slot staleness only breaks ties
        // inside a bucket, so the standing trust ordering still holds.
        bucket_at(&b.0, query.as_of)
            .cmp(&bucket_at(&a.0, query.as_of))
            .then(slot_stale(&a.0).cmp(&slot_stale(&b.0)))
            .then(date_exact(&b.0).cmp(&date_exact(&a.0)))
            .then(b.2.total_cmp(&a.2))
            .then(a.0.id.cmp(&b.0.id))
    });

    // The lanes append into spare slots below `limit`, after the lexical
    // hits are already ordered. Lexical hits keep their order and scores;
    // when they fill the limit the union is empty, so the result is exactly
    // the keyword result. Lane hits carry no lexical relevance (score 0.0).
    // Standing and expansion draw from independent caps, and expansion
    // dedupes against both the lexical hits and the standing lane.
    let lexical_ids: HashSet<EntryId> = scored.iter().map(|(e, _, _)| e.id).collect();
    let standing = standing_lane(pool, revised, query, &lexical_ids);
    let mut existing_ids = lexical_ids;
    existing_ids.extend(standing.iter().map(|(e, _, _)| e.id));
    let expansion = expansion_lane(pool, revised, query, &scored, &existing_ids);

    let binding = query
        .require_evidence
        .then(|| AnswerConstraints::parse(&query.text, date_selected));

    let mut evidence_filtered = 0;
    let hits = scored
        .into_iter()
        .chain(standing)
        .chain(expansion)
        .filter(|(_, st, _)| !query.require_verified || *st == StateAt::Verified)
        .filter(|(e, _, _)| {
            let rejected = binding
                .as_ref()
                .is_some_and(|constraints| !constraints.permits(e));
            evidence_filtered += usize::from(rejected);
            !rejected
        })
        .take(query.limit)
        .map(|(entry, state, score)| Hit {
            entry,
            state,
            score,
        })
        .collect();

    SearchOutcome {
        hits,
        coverage,
        skipped,
        evidence_filtered,
    }
}

/// Corpus statistics for BM25-lite scoring. Entries are already loaded for
/// the candidate pool, so document frequency is free: computing it adds no
/// I/O, only a token count.
#[derive(Clone, Debug, Default)]
pub struct CorpusStats {
    /// Token → number of entries containing it (stemmed vocabulary).
    df: HashMap<String, usize>,
    n: usize,
    avg_len: f64,
}

impl CorpusStats {
    pub fn new<'a>(entries: impl Iterator<Item = &'a Entry>) -> Self {
        let mut stats = Self::default();
        let mut total_len = 0usize;
        for e in entries {
            let mut seen: HashSet<String> = HashSet::new();
            let mut len = 0usize;
            for t in tokenize(&e.summary).into_iter().chain(tokenize(&e.body)) {
                len += 1;
                seen.insert(t);
            }
            for t in seen {
                *stats.df.entry(t).or_default() += 1;
            }
            total_len += len;
            stats.n += 1;
        }
        if stats.n > 0 {
            stats.avg_len = total_len as f64 / stats.n as f64;
        }
        stats
    }

    /// BM25-style idf, positive for every term that occurs in the corpus.
    /// Rare tokens (unique identifiers, one-off technical terms) weigh far
    /// more than vocabulary shared across entries.
    fn idf(&self, token: &str) -> f64 {
        let df = self.df.get(token).copied().unwrap_or(0) as f64;
        let n = self.n as f64;
        ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
    }
}

/// BM25-lite relevance: per-token `idf × tf` with length normalization,
/// summary term frequency weighted double. `0.0` means no signal.
///
/// Deliberately smaller than textbook BM25 (one k1, one b, no
/// query-frequency term since queries are short), but unlike boolean
/// overlap it rewards rare-term hits and stops rewarding past tf
/// saturation.
pub fn score(entry: &Entry, query_tokens: &[String], stats: &CorpusStats) -> f64 {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    const SUMMARY_WEIGHT: f64 = 2.0;
    if query_tokens.is_empty() {
        return 0.0;
    }
    let summary = tokenize(&entry.summary);
    let body = tokenize(&entry.body);
    let len = (summary.len() + body.len()) as f64;
    let norm = 1.0 - B + B * len / stats.avg_len.max(1.0);
    let count = |v: &[String], qt: &str| v.iter().filter(|t| t.as_str() == qt).count() as f64;

    let mut raw = 0.0;
    for qt in query_tokens {
        let tf = SUMMARY_WEIGHT * count(&summary, qt) + count(&body, qt);
        if tf > 0.0 {
            raw += stats.idf(qt) * tf * (K1 + 1.0) / (tf + K1 * norm);
        }
    }
    raw
}

/// The entries whose text may influence scoring — the same set `finish`
/// returns as hits. Hidden entries (rolled back always; superseded tails
/// and contradicted facts under `current_only`) still count toward
/// coverage, but they must not shift IDF for visible results, or
/// retracted history would reorder the present.
pub(crate) fn stats_pool<'a>(
    pool: &'a [Candidate],
    revised: &HashSet<EntryId>,
    query: &Query,
) -> Vec<&'a Entry> {
    visible_indices(pool, revised, query)
        .into_iter()
        .map(|i| &pool[i].entry)
        .collect()
}

/// Indices of candidates visible for scoring and ranking: rolled back
/// always out; superseded tails and contradicted facts out under
/// `current_only`. Shared by `stats_pool` (IDF) and `hybrid_search` (rank
/// channels) so hidden entries shift neither IDF nor RRF ranks.
pub(crate) fn visible_indices(
    pool: &[Candidate],
    revised: &HashSet<EntryId>,
    query: &Query,
) -> Vec<usize> {
    pool.iter()
        .enumerate()
        .filter(|(_, c)| c.state != StateAt::RolledBack)
        .filter(|(_, c)| !query.current_only || !revised.contains(&c.entry.id))
        .filter(|(_, c)| !query.current_only || c.state != StateAt::Contradicted)
        .map(|(i, _)| i)
        .collect()
}

/// Search with coverage accounting for abstention signals.
///
/// Candidate pool = topic-matched entries (score > 0) after temporal filters.
/// Coverage counts run over that pool before truncation, so callers can say
/// "no verified knowledge covers this task" even when hits exist.
pub fn search_with_coverage(store: &Store, query: &Query) -> Result<SearchOutcome> {
    let mut qtokens = tokenize(&query.text);
    qtokens.sort();
    qtokens.dedup();

    let (pool, revised, skipped) = temporal_pool(store, query)?;
    let stats = CorpusStats::new(stats_pool(&pool, &revised, query).into_iter());
    let total = pool.len();

    let matched: Vec<(Entry, StateAt, f64)> = pool
        .iter()
        .filter_map(|c| {
            let s = score(&c.entry, &qtokens, &stats);
            (s > 0.0).then(|| (c.entry.clone(), c.state, s))
        })
        .collect();

    Ok(finish(matched, &pool, &revised, query, total, skipped))
}

/// Search, best hits first. Entries with zero overlap are dropped.
pub fn search(store: &Store, query: &Query) -> Result<Vec<Hit>> {
    search_with_coverage(store, query).map(|o| o.hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn store_with(entries: Vec<Entry>) -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for e in &entries {
            store.create(e).unwrap();
        }
        (dir, store)
    }

    #[test]
    fn tokenizer_drops_stopwords_and_punctuation() {
        assert_eq!(
            tokenize("The checkout QUERY, is slow!"),
            vec!["checkout", "queri", "slow"]
        );
    }

    #[test]
    fn stemmer_collapses_inflections() {
        // The pair that motivated this scorer: verb form meets noun form.
        assert_eq!(stem("migrating"), stem("migration"));
        assert_eq!(stem("migrated"), stem("migration"));
        assert_eq!(stem("retries"), stem("retried"));
        assert_eq!(stem("connections"), stem("connection"));
        assert_eq!(stem("indexes"), stem("index"));
        assert_eq!(stem("slowly"), stem("slow"));
        // A base word shares a stem with every inflection of it, short
        // bases included.
        for family in [
            vec!["code", "codes", "coded", "coding"],
            vec!["use", "used", "using"],
            vec!["file", "files", "filing"],
            vec!["note", "notes", "noted"],
            vec!["type", "types", "typed"],
        ] {
            let base = stem(family[0]);
            for inflection in &family[1..] {
                assert_eq!(
                    stem(inflection),
                    base,
                    "{inflection} must share a stem with {}",
                    family[0]
                );
            }
        }
        assert_eq!(stem("accessing"), stem("access"));
        assert_eq!(stem("accessed"), stem("access"));
        assert_eq!(stem("accesses"), stem("access"));
        assert_eq!(stem("class"), stem("classes"));
        // Plural and trailing-e rules must agree for every e-family:
        // a base word and its plural land on one stem.
        assert_eq!(stem("case"), stem("cases"));
        assert_eq!(stem("response"), stem("responses"));
        assert_eq!(stem("value"), stem("values"));
        assert_eq!(stem("issue"), stem("issues"));
        assert_eq!(stem("queue"), stem("queues"));
        assert_eq!(stem("post"), stem("postings"));
        // Latin -us plurals: "statuses" meets "status" at "statu".
        assert_eq!(stem("status"), stem("statuses"));
        assert_eq!(stem("focus"), stem("focuses"));
        assert_eq!(stem("alias"), stem("aliases"));
        // Short consonant-y bases keep up with their ies plurals.
        assert_eq!(stem("try"), stem("tries"));
        assert_eq!(stem("cry"), stem("cries"));
        // Short words stay untouched.
        assert_eq!(stem("db"), "db");
        assert_eq!(stem("go"), "go");
        // Over-stemming is acceptable when it is consistent on both sides;
        // spot-check two words that must NOT collapse together.
        assert_ne!(stem("cache"), stem("catch"));
    }

    #[test]
    fn inflected_query_matches_uninflected_entry() {
        let mut e = Entry::new("a", EntryKind::Decision, "migration to postgres");
        e.body = "migrated the store".into();
        let (_d, store) = store_with(vec![e]);
        let out = search_with_coverage(&store, &Query::new("migrating", 10)).unwrap();
        assert_eq!(out.hits.len(), 1);
    }

    #[test]
    fn rare_tokens_outweigh_common_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        // "checkout" appears in every entry (df = N, idf ~ 0.4);
        // "ulid" appears once (idf ~ 1.9). A hit on ulid alone must beat a
        // hit on checkout alone.
        for i in 0..4 {
            let mut e = Entry::new("a", EntryKind::Decision, format!("checkout note {i}"));
            e.body = "checkout process details".into();
            store.create(&e).unwrap();
        }
        let mut rare = Entry::new("a", EntryKind::Decision, "ulid checkout");
        rare.body = "unrelated".into();
        store.create(&rare).unwrap();

        let out = search_with_coverage(&store, &Query::new("ulid", 10)).unwrap();
        assert_eq!(out.hits[0].entry.summary, "ulid checkout");
        // The idf contract itself: df=1 outweighs df=5.
        let stats = CorpusStats::new(store.entries().unwrap().iter());
        assert!(
            stats.idf("ulid") > stats.idf("checkout"),
            "df=1 must outweigh df=5: {} vs {}",
            stats.idf("ulid"),
            stats.idf("checkout")
        );
    }

    #[test]
    fn verified_outranks_unverified_at_equal_score() {
        let mut plain = Entry::new("a", EntryKind::Decision, "use postgres queue");
        plain.body = "decided".into();
        let mut verified = Entry::new("a", EntryKind::Decision, "use postgres queue");
        verified.body = "decided".into();
        verified.verify("review", None);
        let (_d, store) = store_with(vec![plain, verified]);

        let out = search_with_coverage(&store, &Query::new("postgres queue", 10)).unwrap();
        assert_eq!(out.hits[0].entry.verification, VerificationState::Verified);
        assert_eq!(out.coverage.verified, 1);
        assert_eq!(out.coverage.unverified, 1);
    }

    #[test]
    fn contradicted_never_outranks_verified_on_same_topic() {
        // A maximally relevant contradicted fact still loses to a minimally
        // relevant verified one: the bucket sorts before the score.
        let mut verified = Entry::new("a", EntryKind::Decision, "postgres for checkout");
        verified.verify("review", None);
        let counter = Entry::new("a", EntryKind::Observation, "counter evidence");
        let mut contradicted = Entry::new(
            "a",
            EntryKind::Decision,
            "postgres postgres postgres checkout checkout",
        );
        contradicted.body = "postgres postgres postgres checkout postgres".into();
        contradicted.contradict(counter.id, "ops", None);
        let (_d, store) = store_with(vec![verified, counter, contradicted]);

        let out = search_with_coverage(&store, &Query::new("postgres checkout", 10)).unwrap();
        assert_eq!(out.hits[0].entry.verification, VerificationState::Verified);
        assert!(matches!(
            out.hits[1].entry.verification,
            VerificationState::Contradicted { .. }
        ));
    }

    #[test]
    fn rolled_back_entries_are_hidden() {
        let mut e = Entry::new("a", EntryKind::Decision, "postgres pooling");
        e.mark_rolled_back("rb");
        let (_d, store) = store_with(vec![e]);
        let out = search_with_coverage(&store, &Query::new("postgres", 10)).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage.hidden_rolled_back, 1);
    }

    #[test]
    fn no_overlap_yields_no_hits_and_zero_matched_coverage() {
        let e = Entry::new("a", EntryKind::Decision, "kubernetes ingress");
        let (_d, store) = store_with(vec![e]);
        let out = search_with_coverage(&store, &Query::new("quantum entanglement", 5)).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage.total_visible(), 0);
        // The entry exists but did not match: total counts the scanned
        // corpus so callers can say "nothing matched" honestly.
        assert_eq!(out.coverage.total, 1);
    }

    #[test]
    fn current_only_keeps_chain_head_and_drops_tails() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let v1 = Entry::new("a", EntryKind::Decision, "use mysql today");
        store.create(&v1).unwrap();
        let mut v2 = Entry::new("a", EntryKind::Decision, "use mysql tomorrow");
        v2.revises = Some(v1.id);
        store.create(&v2).unwrap();

        let mut q = Query::new("mysql", 10);
        q.current_only = true;
        let out = search_with_coverage(&store, &q).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.summary, "use mysql tomorrow");
        assert_eq!(out.coverage.hidden_superseded, 1);
    }

    #[test]
    fn summary_hits_outweigh_body_hits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut in_summary = Entry::new("a", EntryKind::Decision, "migrating off redis");
        in_summary.body = "unrelated".into();
        let mut in_body = Entry::new("a", EntryKind::Observation, "infra note");
        in_body.body = "we are migrating off redis soon".into();
        store.create(&in_summary).unwrap();
        store.create(&in_body).unwrap();

        let out = search_with_coverage(&store, &Query::new("migrating redis", 10)).unwrap();
        assert!(out.hits[0].score > out.hits[1].score);
        assert_eq!(out.hits[0].entry.summary, "migrating off redis");
    }

    #[test]
    fn current_only_hides_tail_when_middle_is_off_topic() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
        store.create(&v1).unwrap();
        let mut v2 = Entry::new("a", EntryKind::Observation, "interim migration note");
        v2.revises = Some(v1.id);
        store.create(&v2).unwrap();
        let mut v3 = Entry::new("a", EntryKind::Decision, "sessions use postgres");
        v3.revises = Some(v2.id);
        store.create(&v3).unwrap();

        let out = search_with_coverage(&store, &Query::new("sessions use", 10).current()).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.summary, "sessions use postgres");
        assert_eq!(out.coverage.hidden_superseded, 1);
    }

    #[test]
    fn coverage_counts_the_pool_before_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for summary in ["alpha checkout", "beta checkout", "gamma checkout"] {
            let mut e = Entry::new("a", EntryKind::Decision, summary);
            e.verify("review", None);
            store.create(&e).unwrap();
        }
        let out = search_with_coverage(&store, &Query::new("checkout", 1)).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.coverage.verified, 3);
        assert_eq!(out.coverage.total_visible(), 3);
    }

    #[test]
    fn current_only_falls_back_when_head_is_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
        store.create(&v1).unwrap();
        let mut v2 = Entry::new("a", EntryKind::Decision, "sessions use postgres");
        v2.revises = Some(v1.id);
        store.create(&v2).unwrap();
        let mut v3 = Entry::new("a", EntryKind::Decision, "sessions use cockroach");
        v3.revises = Some(v2.id);
        v3.mark_rolled_back("ops");
        store.create(&v3).unwrap();

        let out = search_with_coverage(&store, &Query::new("sessions use", 10).current()).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.summary, "sessions use postgres");
        assert_eq!(out.coverage.hidden_rolled_back, 1);
        assert_eq!(out.coverage.hidden_superseded, 1);
    }

    #[test]
    fn coverage_hidden_counts_survive_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let v1 = Entry::new("a", EntryKind::Decision, "checkout tail");
        store.create(&v1).unwrap();
        let mut v2 = Entry::new("a", EntryKind::Decision, "checkout head");
        v2.revises = Some(v1.id);
        v2.verify("review", None);
        store.create(&v2).unwrap();
        let mut rb = Entry::new("a", EntryKind::Decision, "checkout rolled");
        rb.mark_rolled_back("ops");
        store.create(&rb).unwrap();
        let counter = Entry::new("a", EntryKind::Observation, "counter evidence checkout");
        store.create(&counter).unwrap();
        let mut contradicted = Entry::new("a", EntryKind::Observation, "checkout contradicted");
        contradicted.contradict(counter.id, "ops", None);
        store.create(&contradicted).unwrap();
        let extra = Entry::new("a", EntryKind::Decision, "checkout extra verified");
        let mut extra = extra;
        extra.verify("review", None);
        store.create(&extra).unwrap();

        let out = search_with_coverage(&store, &Query::new("checkout", 1).current()).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert!(out.coverage.verified >= 2);
        assert_eq!(out.coverage.hidden_superseded, 1);
        assert_eq!(out.coverage.hidden_rolled_back, 1);
        assert_eq!(out.coverage.contradicted, 1);
    }

    #[test]
    fn empty_and_stopword_query_yields_zero_matched_coverage() {
        let e = Entry::new("a", EntryKind::Decision, "the checkout query");
        let (_d, store) = store_with(vec![e]);
        let out = search_with_coverage(&store, &Query::new("the and", 10)).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage.total_visible(), 0);
        assert_eq!(out.coverage.total, 1);
    }

    #[test]
    fn recency_tiebreaks_within_a_bucket() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut fresh = Entry::new("a", EntryKind::Decision, "checkout deploy note");
        fresh.body = "x".into();
        let mut stale = fresh.clone();
        stale.id = crate::id::EntryId::generate();
        stale.timestamp = Utc::now() - chrono::Duration::days(365);
        store.create(&fresh).unwrap();
        store.create(&stale).unwrap();

        let out = search_with_coverage(&store, &Query::new("checkout deploy", 10)).unwrap();
        assert_eq!(out.hits.len(), 2);
        assert_eq!(out.hits[0].entry.id, fresh.id);
        assert!(out.hits[0].score > out.hits[1].score);
    }

    /// Hidden entries feed coverage only — never IDF. Two stores with
    /// identical visible entries must score identically no matter how
    /// many rolled-back duplicates share the query's tokens.
    #[test]
    fn hidden_entries_do_not_shift_visible_scores() {
        let old = Utc::now() - chrono::Duration::days(3650);
        let mut a = Entry::new("a", EntryKind::Decision, "postgres pooling limits");
        a.timestamp = old;
        a.verify("ops", None);
        let mut b = Entry::new("a", EntryKind::Decision, "postgres timeout notes");
        b.timestamp = old;
        let mut hidden = Entry::new(
            "a",
            EntryKind::Decision,
            "postgres postgres postgres retracted",
        );
        hidden.timestamp = old;
        hidden.mark_rolled_back("ops");

        let (_d1, s1) = store_with(vec![a.clone(), b.clone()]);
        let (_d2, s2) = store_with(vec![a, b, hidden]);
        let q = Query::new("postgres", 10);
        let out1 = search_with_coverage(&s1, &q).unwrap();
        let out2 = search_with_coverage(&s2, &q).unwrap();

        let ids1: Vec<_> = out1.hits.iter().map(|h| h.entry.id).collect();
        let ids2: Vec<_> = out2.hits.iter().map(|h| h.entry.id).collect();
        assert_eq!(ids1, ids2, "hidden entry changed the hit order");
        for (h1, h2) in out1.hits.iter().zip(&out2.hits) {
            assert_eq!(
                h1.score, h2.score,
                "hidden entry shifted the score of {}",
                h1.entry.id
            );
        }
        assert_eq!(out2.coverage.hidden_rolled_back, 1);
    }

    /// SPEC/DESIGN publish trust 1.0/0.75/0.35 and recency at a 90-day
    /// half-life floored at 0.25 — pin both. Score = relevance × trust ×
    /// recency, so equal-text entries reduce each ratio to the constant.
    #[test]
    fn trust_and_recency_constants_hold() {
        // Trust: three same-text entries, one per visible state.
        let old = Utc::now() - chrono::Duration::days(3650);
        let mut verified = Entry::new("a", EntryKind::Decision, "postgres queue note");
        verified.timestamp = old;
        verified.verify("ops", None);
        let mut unverified = Entry::new("a", EntryKind::Decision, "postgres queue note");
        unverified.timestamp = old;
        let mut contradicted = Entry::new("a", EntryKind::Decision, "postgres queue note");
        contradicted.timestamp = old;
        contradicted.contradict(verified.id, "ops", None);
        let (_d, store) = store_with(vec![verified, unverified, contradicted]);

        let out = search_with_coverage(&store, &Query::new("postgres queue", 10)).unwrap();
        assert_eq!(out.hits.len(), 3);
        let (v, u, c) = (&out.hits[0], &out.hits[1], &out.hits[2]);
        assert_eq!(
            v.entry.verification,
            crate::entry::VerificationState::Verified
        );
        assert_eq!(
            u.entry.verification,
            crate::entry::VerificationState::Unverified
        );
        assert!(
            (v.score / u.score - 1.0 / 0.75).abs() < 1e-9,
            "verified/unverified ratio {} != 1.333",
            v.score / u.score
        );
        assert!(
            (u.score / c.score - 0.75 / 0.35).abs() < 1e-9,
            "unverified/contradicted ratio {} != 2.143",
            u.score / c.score
        );

        // Recency: identical text, timestamps exactly at the cutoff and
        // 90 days before it → ratio is exactly one half-life, 0.5.
        let cutoff = Utc::now() - chrono::Duration::days(4000);
        let mut fresh = Entry::new("a", EntryKind::Decision, "postgres queue note");
        fresh.timestamp = cutoff;
        let mut stale = fresh.clone();
        stale.id = crate::id::EntryId::generate();
        stale.timestamp = cutoff - chrono::Duration::days(90);
        let (_d2, s2) = store_with(vec![fresh, stale]);
        let out =
            search_with_coverage(&s2, &Query::new("postgres queue", 10).as_of(cutoff)).unwrap();
        assert_eq!(out.hits.len(), 2);
        assert!(
            (out.hits[1].score / out.hits[0].score - 0.5).abs() < 1e-9,
            "90-day-old entry scored {} of fresh, expected 0.5",
            out.hits[1].score / out.hits[0].score
        );

        // Floor: at 10y and 20y both decay below 0.25, so scores are equal.
        let mut ten = Entry::new("a", EntryKind::Decision, "postgres queue note");
        ten.timestamp = cutoff - chrono::Duration::days(365 * 10);
        let mut twenty = ten.clone();
        twenty.id = crate::id::EntryId::generate();
        twenty.timestamp = cutoff - chrono::Duration::days(365 * 20);
        let (_d3, s3) = store_with(vec![ten, twenty]);
        let out =
            search_with_coverage(&s3, &Query::new("postgres queue", 10).as_of(cutoff)).unwrap();
        assert_eq!(out.hits.len(), 2);
        assert_eq!(
            out.hits[0].score, out.hits[1].score,
            "the 0.25 floor should equalize ancient entries"
        );
    }

    /// The GUNG-18 case. A standing rule that shares no content words with a
    /// task question must surface for a task-shaped query, and a fact-shaped
    /// query over the same store must not pull it in. This is the recall
    /// contract, not a test of the cue tagger or the lane helper.
    #[test]
    fn task_recall_surfaces_standing_constraints_but_fact_recall_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut rule = Entry::new("a", EntryKind::Decision, "Always syntax-highlight code");
        rule.body = "Applies to every snippet the agent returns.".into();
        rule.verify("review", None);
        store.create(&rule).unwrap();
        // A plain fact in the same store, so fact recall has a real answer.
        let fact_entry = Entry::new("a", EntryKind::Decision, "deployment schedule runs Friday");
        store.create(&fact_entry).unwrap();

        // Neither query shares a content word with the standing rule, so
        // BM25 cannot return it for either. The only way it can appear is
        // through the standing-constraint lane.
        let fact = Query::new("what is the deployment schedule", 10);
        let fact_out = search_with_coverage(&store, &fact).unwrap();
        assert!(
            fact_out.hits.iter().any(|h| h.entry.id == fact_entry.id),
            "the fact question finds its answer"
        );
        assert!(
            fact_out.hits.iter().all(|h| h.entry.id != rule.id),
            "a fact question must not pull in the standing rule"
        );

        // "show me a login implementation" is task-shaped. The lane must
        // surface the otherwise-unfindable standing rule.
        let task = Query::new("show me a login implementation", 10);
        let task_out = search_with_coverage(&store, &task).unwrap();
        assert!(
            task_out.hits.iter().any(|h| h.entry.id == rule.id),
            "a task-shaped query must surface the standing rule: {:?}",
            task_out
                .hits
                .iter()
                .map(|h| h.entry.summary.as_str())
                .collect::<Vec<_>>()
        );
        // The rule carries no lexical relevance for this question.
        let lane_hit = task_out
            .hits
            .iter()
            .find(|h| h.entry.id == rule.id)
            .unwrap();
        assert_eq!(lane_hit.score, 0.0);
    }

    /// The lane appends into spare slots; it never evicts or reorders the
    /// lexical hits, even when the result limit is small.
    #[test]
    fn standing_lane_appends_without_displacing_lexical_hits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut lexical = Entry::new("a", EntryKind::Decision, "login form layout");
        lexical.body = "username and password fields".into();
        lexical.verify("review", None);
        store.create(&lexical).unwrap();
        let mut rule = Entry::new("a", EntryKind::Decision, "Always syntax-highlight code");
        rule.verify("review", None);
        store.create(&rule).unwrap();

        let task = Query::new("show me a login implementation", 10);
        let full = search_with_coverage(&store, &task).unwrap();
        let lex_first = full
            .hits
            .iter()
            .position(|h| h.entry.id == lexical.id)
            .unwrap();
        let rule_pos = full
            .hits
            .iter()
            .position(|h| h.entry.id == rule.id)
            .unwrap();
        assert!(
            lex_first < rule_pos,
            "lexical hit must rank before the appended lane hit"
        );

        // A limit of 1 leaves no spare slot: the lexical hit alone is the
        // result, exactly as keyword recall would return.
        let narrow =
            search_with_coverage(&store, &Query::new("show me a login implementation", 1)).unwrap();
        assert_eq!(narrow.hits.len(), 1);
        assert_eq!(narrow.hits[0].entry.id, lexical.id);
    }

    /// The cap is pinned at 4. Six standing rules under a task query yield
    /// at most four lane hits, and no lexical hit is displaced.
    #[test]
    fn standing_lane_is_capped_at_four_and_full_of_lexical_hits_wins() {
        assert_eq!(STANDING_LANE_CAP, 4, "the cap named in the PR must hold");
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        for i in 0..6 {
            let mut rule = Entry::new(
                "a",
                EntryKind::Decision,
                format!("Always use layout variant {i}"),
            );
            rule.verify("review", None);
            store.create(&rule).unwrap();
        }
        let task = Query::new("show me a login implementation", 20);
        let out = search_with_coverage(&store, &task).unwrap();
        assert_eq!(
            out.hits.len(),
            STANDING_LANE_CAP,
            "the capped slice must not exceed 4"
        );

        // With lexical hits filling spare slots, the union completes the
        // result with no displacement of the lexical order.
        let mut lexical = Entry::new("a", EntryKind::Decision, "login form layout");
        lexical.body = "username and password fields".into();
        lexical.verify("review", None);
        store.create(&lexical).unwrap();
        let with_lex =
            search_with_coverage(&store, &Query::new("show me a login implementation", 4)).unwrap();
        assert_eq!(with_lex.hits.len(), 4);
        assert_eq!(
            with_lex.hits[0].entry.id, lexical.id,
            "lexical hit keeps the top rank"
        );
        assert_eq!(
            with_lex.hits.iter().filter(|h| h.score == 0.0).count(),
            3,
            "exactly three lane hits fill the spare slots"
        );
    }

    /// Temporal and knowledge-update queries stay fact-shaped regardless of
    /// wording. The captain's failure case: stuffing generic constraints
    /// into every query can drop temporal hit@10 off .98.
    #[test]
    fn temporal_and_knowledge_update_queries_never_get_the_lane() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut rule = Entry::new("a", EntryKind::Decision, "Always syntax-highlight code");
        rule.verify("review", None);
        store.create(&rule).unwrap();
        let mut fact = Entry::new(
            "a",
            EntryKind::Decision,
            "snippet budget reviews moved to q1",
        );
        fact.verify("review", None);
        store.create(&fact).unwrap();

        for q in [
            Query::new("when did budget reviews move", 10),
            Query::new("show me the current budget reviews", 10),
            Query::new("show me budget reviews", 10).current(),
            Query::new("show me budget reviews", 10).as_of(Utc::now() - chrono::Duration::days(1)),
        ] {
            let out = search_with_coverage(&store, &q).unwrap();
            assert!(
                out.hits.iter().all(|h| h.entry.id != rule.id),
                "{:?} must not receive the standing lane",
                q.text
            );
        }
    }

    /// Unverified standing rules must not steer a task briefing, and a cue
    /// match must never verify an entry by birth. The file on disk is
    /// unchanged after a task query.
    #[test]
    fn unverified_standing_rules_never_steer_task_recall_and_are_not_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let rule = Entry::new("a", EntryKind::Decision, "Always syntax-highlight code");
        store.create(&rule).unwrap();

        let task = Query::new("show me a login implementation", 10);
        let out = search_with_coverage(&store, &task).unwrap();
        assert!(
            out.hits.iter().all(|h| h.entry.id != rule.id),
            "unverified guidance must not enter the lane"
        );
        let on_disk = store.require(rule.id).unwrap();
        assert_eq!(on_disk.verification, VerificationState::Unverified);
        assert!(on_disk.verification_log.is_empty());
    }

    /// `require_verified` is a strict provenance query, so it stays purely
    /// lexical: no standing-constraint guidance enters the fact hit set.
    #[test]
    fn require_verified_queries_stay_lexical() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut rule = Entry::new("a", EntryKind::Decision, "Always syntax-highlight code");
        rule.verify("review", None);
        store.create(&rule).unwrap();

        let gated = search_with_coverage(
            &store,
            &Query::new("show me a login implementation", 10).require_verified(),
        )
        .unwrap();
        assert!(
            gated.hits.iter().all(|h| h.entry.id != rule.id),
            "a strict provenance query must not union standing constraints"
        );
    }

    /// A task query with limit 0 has no spare slots, so the lane must not
    /// append any hits. And lane hits never count as topic-matched coverage:
    /// the abstention signal describes the lexical pool, not the guidance.
    #[test]
    fn task_limit_zero_returns_no_lane_hits_and_lane_hits_leave_coverage_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut rule = Entry::new("a", EntryKind::Decision, "Always syntax-highlight code");
        rule.verify("review", None);
        store.create(&rule).unwrap();

        let zero =
            search_with_coverage(&store, &Query::new("show me a login implementation", 0)).unwrap();
        assert!(
            zero.hits.is_empty(),
            "limit 0 must return no hits, lane included"
        );

        let wide = search_with_coverage(&store, &Query::new("show me a login implementation", 10))
            .unwrap();
        assert_eq!(wide.hits.len(), 1, "the lane surfaces the rule");
        assert_eq!(
            wide.coverage.verified, 0,
            "lane hits are not verified facts on the topic"
        );
        assert_eq!(
            wide.coverage.total_matched(),
            0,
            "coverage counts the lexical pool only"
        );
        assert_eq!(
            wide.coverage.total, 1,
            "the corpus entry still counts in scope"
        );
    }

    /// The tagger matches whole words. "nevertheless" carries "never" as a
    /// substring, but it is not the standing cue "never".
    #[test]
    fn standing_cues_match_whole_words_not_substrings() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut e = Entry::new("a", EntryKind::Decision, "nevertheless the plan proceeded");
        e.verify("review", None);
        store.create(&e).unwrap();

        let out = search_with_coverage(&store, &Query::new("show me a login implementation", 10))
            .unwrap();
        assert!(
            out.hits.iter().all(|h| h.entry.id != e.id),
            "a substring-only cue match must not enter the lane"
        );
    }

    /// A multi-word standing cue cannot be assembled across the summary and
    /// body boundary: neither field contains the phrase on its own.
    #[test]
    fn standing_cues_do_not_span_summary_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut e = Entry::new("a", EntryKind::Decision, "remember from now");
        e.body = "on we review the deploy log".into();
        e.verify("review", None);
        store.create(&e).unwrap();
        // "from now" is not itself a cue; "from now on" is. Splitting the
        // phrase across the two fields must not fabricate the cue.
        let out = search_with_coverage(&store, &Query::new("show me a login implementation", 10))
            .unwrap();
        assert!(
            out.hits.iter().all(|h| h.entry.id != e.id),
            "a cue phrase spanning the field boundary must not count"
        );
    }

    /// When the cap binds, the surviving four are chosen deterministically:
    /// newest by the entry timestamp first, then the EntryId as a stable
    /// tie-break. ULIDs carry only millisecond time plus 80 random bits, so
    /// entries written in one millisecond are NOT ordered by creation — the
    /// tie-break, not creation order, decides. Verified rules also precede a
    /// contradicted one regardless of creation order.
    #[test]
    fn lane_cap_keeps_a_deterministic_slice_and_orders_by_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        // Six verified rules sharing one timestamp: the id tie-break alone
        // decides which four the cap keeps. Asserting "the four last
        // created" would be a false premise on a fast runner.
        let tied = Utc::now() - chrono::Duration::days(1);
        let mut rules = Vec::new();
        for i in 0..6 {
            let mut r = Entry::new("a", EntryKind::Decision, format!("Always use layout {i}"));
            r.timestamp = tied;
            r.verify("review", None);
            store.create(&r).unwrap();
            rules.push(r);
        }
        let out = search_with_coverage(&store, &Query::new("show me a login implementation", 20))
            .unwrap();
        let survivors: Vec<_> = out.hits.iter().map(|h| h.entry.id).collect();
        let mut expected: Vec<_> = rules.iter().map(|r| r.id).collect();
        expected.sort_by(|a, b| b.cmp(a));
        expected.truncate(STANDING_LANE_CAP);
        assert_eq!(
            survivors, expected,
            "an equal-timestamp tie is broken by descending EntryId"
        );

        // Distinct timestamps: the newer rule wins over an older one even
        // when the older one happens to carry a higher id (pin the timestamp
        // key, not just the id tie-break).
        let dir_ts = tempfile::tempdir().unwrap();
        let store_ts = Store::open(dir_ts.path()).unwrap();
        let mut older = Entry::new("a", EntryKind::Decision, "Always use the old layout");
        older.timestamp = Utc::now() - chrono::Duration::days(10);
        older.verify("review", None);
        // Give the older rule the higher id so id-descending alone would
        // rank it first; the timestamp key must overrule that.
        let mut newer = Entry::new("a", EntryKind::Decision, "Always use the new layout");
        newer.timestamp = Utc::now();
        newer.verify("review", None);
        if newer.id > older.id {
            std::mem::swap(&mut older.id, &mut newer.id);
        }
        store_ts.create(&older).unwrap();
        store_ts.create(&newer).unwrap();
        let ts_out =
            search_with_coverage(&store_ts, &Query::new("show me a login implementation", 10))
                .unwrap();
        assert_eq!(
            ts_out.hits[0].entry.id, newer.id,
            "newest by timestamp ranks first even with a lower id"
        );

        // A contradicted rule must not reach the lane at all; only the
        // verified rule is guidance.
        let dir2 = tempfile::tempdir().unwrap();
        let store2 = Store::open(dir2.path()).unwrap();
        let mut verified = Entry::new("a", EntryKind::Decision, "Always use dark mode");
        verified.verify("review", None);
        store2.create(&verified).unwrap();
        let counter = Entry::new("a", EntryKind::Observation, "counter evidence");
        store2.create(&counter).unwrap();
        let mut contradicted = Entry::new("a", EntryKind::Decision, "Always use light mode");
        contradicted.contradict(counter.id, "ops", None);
        store2.create(&contradicted).unwrap();
        let mixed =
            search_with_coverage(&store2, &Query::new("show me a login implementation", 10))
                .unwrap();
        let lane: Vec<_> = mixed
            .hits
            .iter()
            .filter(|h| h.score == 0.0)
            .map(|h| h.entry.id)
            .collect();
        assert_eq!(
            lane,
            vec![verified.id],
            "only verified guidance is in the lane"
        );
    }

    /// A standing rule that also matches the task lexically appears once, as
    /// a scored hit, not twice (once scored and once as a 0.0 lane hit).
    #[test]
    fn lexical_standing_match_appears_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut rule = Entry::new("a", EntryKind::Decision, "Always use login layout");
        rule.verify("review", None);
        store.create(&rule).unwrap();

        let out = search_with_coverage(&store, &Query::new("show me login layout", 10)).unwrap();
        let appearances: Vec<_> = out.hits.iter().filter(|h| h.entry.id == rule.id).collect();
        assert_eq!(appearances.len(), 1, "the rule must not be duplicated");
        assert!(
            appearances[0].score > 0.0,
            "the lexical hit keeps its score"
        );
    }

    /// A fact cue wins over a task cue, so a mixed query gets no lane while
    /// the same store's pure task query does.
    #[test]
    fn fact_cue_wins_over_task_cue_in_mixed_queries() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut rule = Entry::new("a", EntryKind::Decision, "Always syntax-highlight code");
        rule.verify("review", None);
        store.create(&rule).unwrap();
        let fact = Entry::new("a", EntryKind::Decision, "decision log about login");
        store.create(&fact).unwrap();

        let mixed = Query::new("please write up what was decided about login", 10);
        let mixed_out = search_with_coverage(&store, &mixed).unwrap();
        assert!(
            mixed_out.hits.iter().all(|h| h.entry.id != rule.id),
            "a fact cue must keep the lane out of a mixed query"
        );

        let pure = Query::new("show me a login implementation", 10);
        let pure_out = search_with_coverage(&store, &pure).unwrap();
        assert!(
            pure_out.hits.iter().any(|h| h.entry.id == rule.id),
            "the pure task query still gets the lane"
        );
    }

    /// Single-word task verbs match common inflections. Multi-word cues stay
    /// exact, so "building" reaches the lane without loosening phrase cues.
    #[test]
    fn inflected_task_verbs_reach_the_standing_lane() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut rule = Entry::new("a", EntryKind::Decision, "Always syntax-highlight code");
        rule.verify("review", None);
        store.create(&rule).unwrap();

        for text in [
            "building a login page",
            "writing a login page",
            "creating a login page",
        ] {
            let out = search_with_coverage(&store, &Query::new(text, 10)).unwrap();
            assert!(
                out.hits.iter().any(|h| h.entry.id == rule.id),
                "{text:?} must be task-shaped"
            );
        }
    }

    /// The AEV-43 contract. A stale verified tail carries more query tokens
    /// than its newer live verified head, so BM25 alone ranks the tail first.
    /// A plain present-time query must resolve the `revises` chain as one
    /// slot and put the latest live verified member first, without writing
    /// anything and without hiding the tail from a non-current read.
    fn verified_slot_pair() -> (tempfile::TempDir, Store, EntryId, EntryId) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut stale = Entry::new("a", EntryKind::Decision, "12 commits merged into main");
        stale.body = "12 commits merged into main total count".into();
        stale.timestamp = Utc::now() - chrono::Duration::days(30);
        stale.verify("archive", None);
        store.create(&stale).unwrap();
        let mut head = Entry::new("a", EntryKind::Decision, "main now has 19 commits");
        head.body = "merge count updated".into();
        head.timestamp = Utc::now() - chrono::Duration::days(1);
        head.revises = Some(stale.id);
        head.verify("archive", None);
        store.create(&head).unwrap();
        (dir, store, stale.id, head.id)
    }

    #[test]
    fn latest_live_verified_slot_member_outranks_stale_tail_on_current_recall() {
        let (_d, store, stale_id, head_id) = verified_slot_pair();
        let out =
            search_with_coverage(&store, &Query::new("commits merged into main", 10)).unwrap();
        assert_eq!(
            out.hits[0].entry.id, head_id,
            "the latest live head ranks first"
        );
        assert_eq!(
            out.hits[1].entry.id, stale_id,
            "the tail stays visible in a plain read"
        );
        assert_eq!(out.hits.len(), 2);
        assert_eq!(out.coverage.verified, 2);
        // No write: both entries keep the states the store already held.
        assert_eq!(store.require(head_id).unwrap().revises, Some(stale_id));
        assert_eq!(store.require(stale_id).unwrap().revises, None);
    }

    /// The slot head is the link target's revises parent: the member with no
    /// live `revises` parent. Plain and `--current` views must agree on it in
    /// both link directions, so a link whose arrow points at the newer entry
    /// cannot make the two views name opposite heads.
    fn slot_pair(direction: &str) -> (tempfile::TempDir, Store, EntryId, EntryId) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut older = Entry::new("a", EntryKind::Decision, "budget is 100 dollars");
        older.timestamp = Utc::now() - chrono::Duration::days(30);
        older.verify("archive", None);
        let mut newer = Entry::new("a", EntryKind::Decision, "budget is 250 dollars");
        newer.timestamp = Utc::now() - chrono::Duration::days(1);
        newer.verify("archive", None);
        match direction {
            // Well-formed: newer.revises = older. `head` = newer.
            "newer_revises_older" => newer.revises = Some(older.id),
            // Inverted: older.revises = newer. `head` = older (no parent).
            "older_revises_newer" => older.revises = Some(newer.id),
            _ => unreachable!(),
        }
        // The lexical advantage goes to the non-head, so only slot
        // resolution can put the head first.
        if direction == "newer_revises_older" {
            older.body = "budget dollars budget dollars budget dollars".into();
        } else {
            newer.body = "budget dollars budget dollars budget dollars".into();
        }
        // Write-time validation requires the `revises` target to exist, so
        // the target goes first in each direction.
        if direction == "newer_revises_older" {
            store.create(&older).unwrap();
            store.create(&newer).unwrap();
            (dir, store, newer.id, older.id)
        } else {
            store.create(&newer).unwrap();
            store.create(&older).unwrap();
            (dir, store, older.id, newer.id)
        }
    }

    #[test]
    fn plain_and_current_views_agree_on_slot_head_in_both_link_directions() {
        for direction in ["newer_revises_older", "older_revises_newer"] {
            let (_d, store, head, tail) = slot_pair(direction);
            let plain = search_with_coverage(&store, &Query::new("budget dollars", 10)).unwrap();
            assert_eq!(plain.hits[0].entry.id, head, "{direction}: plain head");
            let current =
                search_with_coverage(&store, &Query::new("budget dollars", 10).current()).unwrap();
            assert_eq!(current.hits.len(), 1, "{direction}: current hides the tail");
            assert_eq!(current.hits[0].entry.id, head, "{direction}: current head");
            assert!(!current.hits.iter().any(|h| h.entry.id == tail));
            assert_eq!(current.coverage.hidden_superseded, 1);
        }
    }

    /// With equal timestamps, the shared head definition still decides
    /// deterministically: the member with no live `revises` parent wins, and
    /// both current views agree regardless of scan order or lexical score.
    #[test]
    fn slot_resolution_is_deterministic_at_equal_timestamps() {
        let tied = Utc::now() - chrono::Duration::days(1);
        let mut tail = Entry::new("a", EntryKind::Decision, "quota is 40 units");
        tail.timestamp = tied;
        tail.verify("archive", None);
        let mut head = Entry::new("a", EntryKind::Decision, "quota is 90 units");
        head.timestamp = tied;
        head.verify("archive", None);
        // The head carries `revises` pointing at the tail, so `revised`
        // holds the tail and the head wins. Give the tail the lexical
        // advantage so only slot resolution puts the head first, then repeat
        // to prove the order is stable at equal timestamps.
        head.revises = Some(tail.id);
        tail.body = "quota units quota units quota units quota units".into();
        // The link target must exist first for write-time validation.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create(&tail).unwrap();
        store.create(&head).unwrap();

        for _ in 0..3 {
            let plain = search_with_coverage(&store, &Query::new("quota units", 10)).unwrap();
            assert_eq!(plain.hits[0].entry.id, head.id, "plain head is stable");
            let current =
                search_with_coverage(&store, &Query::new("quota units", 10).current()).unwrap();
            assert_eq!(current.hits.len(), 1);
            assert_eq!(current.hits[0].entry.id, head.id, "current head agrees");
        }
    }

    /// `as_of` keeps historical semantics: the slot winner is the newest
    /// verified member that existed at the cutoff, not the present head. A
    /// point-in-time read must not be reordered by present-day slot state.
    #[test]
    fn as_of_keeps_historical_slot_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let t0 = Utc::now() - chrono::Duration::days(30);
        let t1 = Utc::now() - chrono::Duration::days(10);
        let mut stale = Entry::new("a", EntryKind::Decision, "deploy window 2am");
        stale.timestamp = t0;
        stale.verify("archive", None);
        store.create(&stale).unwrap();
        let mut head = Entry::new("a", EntryKind::Decision, "deploy window 4am");
        head.timestamp = t1;
        head.body = "deploy window 2am deploy window 2am".into();
        head.revises = Some(stale.id);
        head.verify("archive", None);
        store.create(&head).unwrap();

        // Present time: the head wins regardless of lexical score.
        let now = search_with_coverage(&store, &Query::new("deploy window", 10)).unwrap();
        assert_eq!(now.hits[0].entry.id, head.id);

        // Before the head existed, only the tail is in scope at all.
        let cutoff = t0 + chrono::Duration::days(1);
        let past =
            search_with_coverage(&store, &Query::new("deploy window", 10).as_of(cutoff)).unwrap();
        assert_eq!(past.hits.len(), 1);
        assert_eq!(past.hits[0].entry.id, stale.id);
    }

    /// Only a live verified member resolves a slot. An all-unverified chain
    /// keeps the ordinary trust-weighted ordering, so an unverified proposal
    /// cannot reorder history by being the newest link. This is the case
    /// that isolates the rule: with no verified member, the stale wording
    /// still wins on BM25 score.
    #[test]
    fn slot_resolution_requires_a_verified_member() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let tied = Utc::now() - chrono::Duration::days(1);
        let mut stale = Entry::new("a", EntryKind::Decision, "region zeta marker");
        stale.timestamp = tied;
        store.create(&stale).unwrap();
        let mut head = Entry::new("a", EntryKind::Decision, "region plain");
        head.timestamp = tied;
        head.revises = Some(stale.id);
        store.create(&head).unwrap();

        // The stale wording owns the rare "zeta" token, so BM25 ranks it
        // first. With no verified member in the slot, nothing demotes it.
        let out = search_with_coverage(&store, &Query::new("region zeta", 10)).unwrap();
        assert_eq!(
            out.hits[0].entry.id, stale.id,
            "no verified winner: BM25 decides"
        );
    }

    /// A verified stale tail keeps its trust bucket even when the newer head
    /// is an unverified proposal. Verified knowledge is never demoted below
    /// hearsay by slot resolution.
    #[test]
    fn unverified_head_never_displaces_a_verified_slot_tail() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut stale = Entry::new("a", EntryKind::Decision, "region is us-east");
        stale.timestamp = Utc::now() - chrono::Duration::days(30);
        stale.verify("archive", None);
        store.create(&stale).unwrap();
        let mut head = Entry::new("a", EntryKind::Decision, "region is eu-west");
        head.timestamp = Utc::now() - chrono::Duration::days(1);
        head.body = "region us-east region us-east".into();
        head.revises = Some(stale.id);
        store.create(&head).unwrap();

        let out = search_with_coverage(&store, &Query::new("region", 10)).unwrap();
        assert_eq!(out.hits[0].entry.id, stale.id);
    }

    /// A rolled-back head falls back to the last live member, matching the
    /// `current_only` rule. A rollback record's `revises` is a restore, not
    /// a revision, so it never becomes a slot head.
    #[test]
    fn slot_resolution_falls_back_when_head_is_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
        v1.timestamp = Utc::now() - chrono::Duration::days(30);
        v1.verify("archive", None);
        store.create(&v1).unwrap();
        let mut v2 = Entry::new("a", EntryKind::Decision, "sessions use postgres");
        v2.timestamp = Utc::now() - chrono::Duration::days(10);
        v2.revises = Some(v1.id);
        v2.verify("archive", None);
        store.create(&v2).unwrap();
        let mut v3 = Entry::new("a", EntryKind::Decision, "sessions use cockroach");
        v3.timestamp = Utc::now() - chrono::Duration::days(1);
        v3.body = "sessions use mysql sessions use mysql".into();
        v3.revises = Some(v2.id);
        v3.mark_rolled_back("ops");
        store.create(&v3).unwrap();

        let out = search_with_coverage(&store, &Query::new("sessions use", 10)).unwrap();
        // The rolled-back head is hidden; v2 is the latest live member.
        assert_eq!(out.hits[0].entry.id, v2.id);
        assert_eq!(out.coverage.hidden_rolled_back, 1);
        // The shared `revised` set excludes the rolled-back head's link, so
        // the strict current view names the same live head.
        let current =
            search_with_coverage(&store, &Query::new("sessions use", 10).current()).unwrap();
        assert_eq!(current.hits[0].entry.id, v2.id);
    }

    /// A `revises` cycle degrades safely: the shared link-target rule marks
    /// both members as tails, both current views still return results, and
    /// neither view crashes or hides the whole slot.
    #[test]
    fn revises_cycle_degrades_without_hiding_the_slot_in_both_views() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut a = Entry::new("a", EntryKind::Decision, "quota is 40 units");
        a.verify("archive", None);
        let mut b = Entry::new("a", EntryKind::Decision, "quota is 90 units");
        b.verify("archive", None);
        // A direct cycle. Write-time validation of `revises` needs the
        // target to exist, so create `a` first, then `b` pointing at `a`,
        // then rewrite `a` to point back at `b`.
        store.create(&a).unwrap();
        b.revises = Some(a.id);
        store.create(&b).unwrap();
        a.revises = Some(b.id);
        store.update_with(&a, &|_| Ok(true)).unwrap();

        let plain = search_with_coverage(&store, &Query::new("quota units", 10)).unwrap();
        assert_eq!(plain.hits.len(), 2);
        let current =
            search_with_coverage(&store, &Query::new("quota units", 10).current()).unwrap();
        assert_eq!(current.hits.len(), 0, "both are link targets, both hidden");
        assert_eq!(current.coverage.hidden_superseded, 2);
    }

    /// `current_only` still hides every tail and counts it as hidden. The
    /// slot demotion must not change that view's contract.
    #[test]
    fn current_only_slot_view_still_hides_tails() {
        let (_d, store, stale_id, head_id) = verified_slot_pair();
        let out = search_with_coverage(
            &store,
            &Query::new("commits merged into main", 10).current(),
        )
        .unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.id, head_id);
        assert_eq!(out.coverage.hidden_superseded, 1);
        assert!(!out.hits.iter().any(|h| h.entry.id == stale_id));
    }

    /// A plain query with limit 1 must still return the live head: the slot
    /// tie-break runs in `sort_by` before `.take(limit)`, so an early
    /// truncation that kept the stale tail's higher BM25 score is caught.
    #[test]
    fn plain_slot_query_with_limit_one_returns_the_live_head() {
        let (_d, store, _stale_id, head_id) = verified_slot_pair();
        let out = search_with_coverage(&store, &Query::new("commits merged into main", 1)).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.id, head_id);
    }

    /// An `as_of` cutoff after both members exist keeps historical order:
    /// both entries are in scope, but a point-in-time read never applies the
    /// present-day slot demotion, so the tail's BM25 advantage stands. A
    /// plain present-time query over the same store ranks the head first.
    #[test]
    fn as_of_covering_the_full_chain_keeps_historical_order() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let t0 = Utc::now() - chrono::Duration::days(30);
        let t1 = Utc::now() - chrono::Duration::days(10);
        let mut stale = Entry::new("a", EntryKind::Decision, "12 commits merged into main");
        stale.body = "12 commits merged into main total count".into();
        stale.timestamp = t0;
        stale.verify("archive", None);
        store.create(&stale).unwrap();
        let mut head = Entry::new("a", EntryKind::Decision, "main now has 19 commits");
        head.body = "merge count updated".into();
        head.timestamp = t1;
        head.revises = Some(stale.id);
        head.verify("archive", None);
        store.create(&head).unwrap();

        let cutoff = t1 + chrono::Duration::days(1);
        let past = search_with_coverage(
            &store,
            &Query::new("commits merged into main", 10).as_of(cutoff),
        )
        .unwrap();
        assert_eq!(past.hits.len(), 2, "both generations are in scope");
        assert_eq!(past.hits[0].entry.id, stale.id, "history keeps BM25 order");

        let now =
            search_with_coverage(&store, &Query::new("commits merged into main", 10)).unwrap();
        assert_eq!(
            now.hits[0].entry.id, head.id,
            "present time promotes the head"
        );
    }

    /// A three-member verified chain A<-B<-C is one slot. The oldest tail
    /// owns the rare query token, so BM25 would rank a tail first; a plain
    /// query must demote both tails below the live head, and `--current`
    /// must hide both.
    #[test]
    fn plain_and_current_views_demote_both_tails_in_a_three_member_chain() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql zephyr");
        v1.body = "sessions use mysql zephyr sessions use mysql zephyr".into();
        v1.timestamp = Utc::now() - chrono::Duration::days(30);
        v1.verify("archive", None);
        store.create(&v1).unwrap();
        let mut v2 = Entry::new("a", EntryKind::Decision, "sessions use postgres");
        v2.timestamp = Utc::now() - chrono::Duration::days(10);
        v2.revises = Some(v1.id);
        v2.verify("archive", None);
        store.create(&v2).unwrap();
        let mut v3 = Entry::new("a", EntryKind::Decision, "sessions use cockroach");
        v3.timestamp = Utc::now() - chrono::Duration::days(1);
        v3.revises = Some(v2.id);
        v3.verify("archive", None);
        store.create(&v3).unwrap();

        let plain = search_with_coverage(&store, &Query::new("sessions use zephyr", 10)).unwrap();
        assert_eq!(plain.hits[0].entry.id, v3.id, "the live head ranks first");
        let head_pos = plain.hits.iter().position(|h| h.entry.id == v3.id).unwrap();
        for tail in [v1.id, v2.id] {
            let pos = plain.hits.iter().position(|h| h.entry.id == tail).unwrap();
            assert!(pos > head_pos, "tail {tail} is demoted below the head");
        }

        let current =
            search_with_coverage(&store, &Query::new("sessions use zephyr", 10).current()).unwrap();
        assert_eq!(current.hits.len(), 1);
        assert_eq!(current.hits[0].entry.id, v3.id);
        assert_eq!(current.coverage.hidden_superseded, 2);
    }

    /// The `slot_stale` tie-break is global inside the verified bucket: a
    /// stale tail loses to every non-stale verified hit, not just its own
    /// slot head. An unrelated verified entry with a weaker BM25 score than
    /// the tail must still rank above it.
    #[test]
    fn stale_slot_tail_ranks_below_every_non_stale_verified_hit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut stale = Entry::new("a", EntryKind::Decision, "12 commits merged into main");
        stale.body = "12 commits merged into main total count".into();
        stale.timestamp = Utc::now() - chrono::Duration::days(30);
        stale.verify("archive", None);
        store.create(&stale).unwrap();
        let mut head = Entry::new("a", EntryKind::Decision, "main now has 19 commits");
        head.body = "merge count updated".into();
        head.timestamp = Utc::now() - chrono::Duration::days(1);
        head.revises = Some(stale.id);
        head.verify("archive", None);
        store.create(&head).unwrap();
        // Unrelated verified fact, same topic but a weaker lexical match
        // than the stale tail. It has no slot, so it is not stale.
        let mut other = Entry::new("a", EntryKind::Decision, "commits note");
        other.verify("archive", None);
        store.create(&other).unwrap();

        let out =
            search_with_coverage(&store, &Query::new("commits merged into main", 10)).unwrap();
        let pos = |id| out.hits.iter().position(|h| h.entry.id == id).unwrap();
        assert!(
            pos(head.id) < pos(stale.id),
            "the live head outranks the tail"
        );
        assert!(
            pos(other.id) < pos(stale.id),
            "an unrelated verified hit also outranks the stale tail"
        );
    }

    /// A verified neighbor with no query-token overlap becomes a hit only
    /// when it shares an entity string with a positive lexical seed. The
    /// separate store keeps the same seed but puts the neighbor off-key, so
    /// the key is the only thing that recovers it.
    #[test]
    fn entity_neighbor_without_query_token_overlap_is_recovered_as_a_verified_expansion_hit() {
        let mut seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        seed.entities = vec!["Harbor Trip".into()];
        seed.verify("review", None);

        let mut neighbor = Entry::new("a", EntryKind::Decision, "quartz ledger");
        neighbor.body = "[[Harbor Trip]] crossed the quay".into();
        neighbor.verify("review", None);

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create(&seed).unwrap();
        store.create(&neighbor).unwrap();

        let query = Query::new("zephyr release", 10);
        let out = search_with_coverage(&store, &query).unwrap();
        let hit = out.hits.iter().find(|h| h.entry.id == neighbor.id).unwrap();
        assert_eq!(hit.score, 0.0, "an expansion hit carries no relevance");
        assert_eq!(hit.state, StateAt::Verified);
        assert_eq!(out.hits[0].entry.id, seed.id, "the seed keeps the top rank");
        assert_eq!(out.coverage.verified, 1, "the seed alone is topic-matched");

        let off_key_dir = tempfile::tempdir().unwrap();
        let off_key_store = Store::open(off_key_dir.path()).unwrap();
        let mut off_key = neighbor.clone();
        off_key.id = EntryId::generate();
        off_key.entities = vec!["Different Marina".into()];
        off_key.body = "quartz ledger".into();
        off_key_store.create(&seed).unwrap();
        off_key_store.create(&off_key).unwrap();
        let off_key_out = search_with_coverage(&off_key_store, &query).unwrap();
        assert!(
            off_key_out.hits.iter().all(|h| h.entry.id != off_key.id),
            "a neighbor sharing no key stays invisible"
        );
    }

    /// A verified neighbor sharing `session_id` with a positive seed is
    /// recovered under `current()`, the GUNG-21 harness shape, even with no
    /// query-token overlap.
    #[test]
    fn session_neighbors_without_query_token_overlap_are_recovered_after_a_lexical_seed() {
        let mut seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        seed.session_id = Some("session-harbor".into());
        seed.verify("review", None);
        let mut neighbor = Entry::new("a", EntryKind::Decision, "quartz ledger");
        neighbor.session_id = Some("session-harbor".into());
        neighbor.verify("review", None);

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create(&seed).unwrap();
        store.create(&neighbor).unwrap();

        let out =
            search_with_coverage(&store, &Query::new("zephyr release", 10).current()).unwrap();
        let hit = out.hits.iter().find(|h| h.entry.id == neighbor.id).unwrap();
        assert_eq!(hit.score, 0.0);
        assert_eq!(hit.state, StateAt::Verified);
        assert_eq!(
            out.coverage.verified, 1,
            "the neighbor is not topic-matched"
        );
        assert_eq!(out.coverage.total_matched(), 1);
    }

    /// The second cap holds at six, expansion never displaces a lexical hit,
    /// and the surviving slice is deterministic under an equal-timestamp tie.
    #[test]
    fn expansion_lane_is_capped_at_six_and_never_displaces_lexical_hits() {
        assert_eq!(EXPANSION_LANE_CAP, 6, "the cap named in the PR must hold");
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let mut seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        seed.session_id = Some("session-harbor".into());
        seed.verify("review", None);
        store.create(&seed).unwrap();

        let tied = Utc::now() - chrono::Duration::days(1);
        let mut neighbors = Vec::new();
        for i in 0..8 {
            let mut n = Entry::new("a", EntryKind::Decision, format!("quartz ledger {i}"));
            n.session_id = Some("session-harbor".into());
            n.timestamp = tied;
            n.verify("review", None);
            store.create(&n).unwrap();
            neighbors.push(n);
        }

        let wide = search_with_coverage(&store, &Query::new("zephyr release", 20)).unwrap();
        let appended: Vec<_> = wide
            .hits
            .iter()
            .filter(|h| h.score == 0.0)
            .map(|h| h.entry.id)
            .collect();
        assert_eq!(appended.len(), EXPANSION_LANE_CAP);
        assert_eq!(wide.hits[0].entry.id, seed.id, "lexical hits come first");
        assert!(
            wide.hits
                .iter()
                .take_while(|h| h.score > 0.0)
                .any(|h| h.entry.id == seed.id),
            "every lexical hit precedes the appended hits"
        );
        let mut expected: Vec<_> = neighbors.iter().map(|n| n.id).collect();
        expected.sort_by(|a, b| b.cmp(a));
        expected.truncate(EXPANSION_LANE_CAP);
        assert_eq!(appended, expected, "descending EntryId breaks the tie");

        let narrow = search_with_coverage(&store, &Query::new("zephyr release", 1)).unwrap();
        assert_eq!(narrow.hits.len(), 1);
        assert_eq!(narrow.hits[0].entry.id, seed.id);
    }

    /// Unverified and contradicted neighbors share the seed's key but never
    /// enter the hit set, and the canonical files keep their state.
    #[test]
    fn unverified_and_contradicted_neighbors_do_not_steer_briefings() {
        let mut seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        seed.session_id = Some("session-harbor".into());
        seed.verify("review", None);

        let mut unverified = Entry::new("a", EntryKind::Decision, "quartz ledger");
        unverified.session_id = Some("session-harbor".into());

        let counter = Entry::new("a", EntryKind::Observation, "counter evidence");
        let mut contradicted = Entry::new("a", EntryKind::Decision, "onyx register");
        contradicted.session_id = Some("session-harbor".into());
        contradicted.contradict(counter.id, "ops", None);

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create(&seed).unwrap();
        store.create(&unverified).unwrap();
        store.create(&counter).unwrap();
        store.create(&contradicted).unwrap();

        let out = search_with_coverage(&store, &Query::new("zephyr release", 10)).unwrap();
        assert!(out.hits.iter().all(|h| h.entry.id != unverified.id));
        assert!(out.hits.iter().all(|h| h.entry.id != contradicted.id));
        assert_eq!(
            store.require(unverified.id).unwrap().verification,
            VerificationState::Unverified
        );
        assert!(matches!(
            store.require(contradicted.id).unwrap().verification,
            VerificationState::Contradicted { .. }
        ));
    }

    /// Expansion appends after the ordered lexical hits, so it cannot reorder
    /// latest-slot resolution. A stale tail sharing more query tokens stays
    /// behind the head on a plain read, `current()` hides the tails while a
    /// verified off-topic neighbor still appends, `as_of` keeps the
    /// historical order, and `require_verified` stays purely lexical.
    #[test]
    fn fact_current_only_and_as_of_queries_keep_latest_slot_order_with_expansion() {
        let t0 = Utc::now() - chrono::Duration::days(30);
        let t1 = Utc::now() - chrono::Duration::days(10);
        let t2 = Utc::now() - chrono::Duration::days(1);

        let mut v1 = Entry::new("a", EntryKind::Decision, "zephyr release zephyr");
        v1.body = "zephyr release zephyr release".into();
        v1.timestamp = t0;
        v1.session_id = Some("session-harbor".into());
        v1.verify("archive", None);
        v1.verification_log[0].timestamp = t0;
        let mut v2 = Entry::new("a", EntryKind::Decision, "zephyr release");
        v2.timestamp = t1;
        v2.revises = Some(v1.id);
        v2.session_id = Some("session-harbor".into());
        v2.verify("archive", None);
        v2.verification_log[0].timestamp = t1;
        let mut v3 = Entry::new("a", EntryKind::Decision, "zephyr release schedule");
        v3.timestamp = t2;
        v3.revises = Some(v2.id);
        v3.session_id = Some("session-harbor".into());
        v3.verify("archive", None);
        v3.verification_log[0].timestamp = t2;
        let mut neighbor = Entry::new("a", EntryKind::Decision, "quartz ledger");
        neighbor.session_id = Some("session-harbor".into());
        neighbor.verify("review", None);

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create(&v1).unwrap();
        store.create(&v2).unwrap();
        store.create(&v3).unwrap();
        store.create(&neighbor).unwrap();

        let plain = search_with_coverage(&store, &Query::new("zephyr release", 10)).unwrap();
        assert_eq!(plain.hits[0].entry.id, v3.id, "the live head ranks first");
        let head_pos = plain.hits.iter().position(|h| h.entry.id == v3.id).unwrap();
        for tail in [v1.id, v2.id] {
            let pos = plain.hits.iter().position(|h| h.entry.id == tail).unwrap();
            assert!(pos > head_pos, "tail {tail} stays behind the head");
        }

        let current =
            search_with_coverage(&store, &Query::new("zephyr release", 10).current()).unwrap();
        assert_eq!(current.hits.len(), 2, "head plus the off-topic neighbor");
        assert_eq!(current.hits[0].entry.id, v3.id);
        assert_eq!(current.hits[1].entry.id, neighbor.id);
        assert_eq!(current.coverage.hidden_superseded, 2);

        let as_of = search_with_coverage(
            &store,
            &Query::new("zephyr release", 10).as_of(t1 + chrono::Duration::seconds(1)),
        )
        .unwrap();
        assert!(
            as_of.hits.iter().any(|h| h.entry.id == v1.id)
                && as_of.hits.iter().any(|h| h.entry.id == v2.id),
            "as_of keeps both members that existed before the latest revision"
        );
        assert!(
            !as_of.hits.iter().any(|h| h.entry.id == v3.id),
            "the latest revision did not exist at this cutoff"
        );
        assert!(
            !as_of.hits.iter().any(|h| h.entry.id == neighbor.id),
            "the neighbor is not yet verified at this cutoff"
        );

        let gated =
            search_with_coverage(&store, &Query::new("zephyr release", 10).require_verified())
                .unwrap();
        assert!(
            gated.hits.iter().all(|h| h.entry.id != neighbor.id),
            "a strict provenance query adds no off-topic neighbors"
        );
    }

    /// Lane hits never count as topic-matched coverage, and a zero limit
    /// leaves no spare slot for any lane.
    #[test]
    fn expansion_lane_hits_leave_coverage_unchanged_and_limit_zero_returns_nothing() {
        let mut seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        seed.session_id = Some("session-harbor".into());
        seed.verify("review", None);
        let mut neighbor = Entry::new("a", EntryKind::Decision, "quartz ledger");
        neighbor.session_id = Some("session-harbor".into());
        neighbor.verify("review", None);

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.create(&seed).unwrap();
        store.create(&neighbor).unwrap();

        let wide = search_with_coverage(&store, &Query::new("zephyr release", 10)).unwrap();
        assert_eq!(wide.hits.len(), 2);
        assert_eq!(wide.coverage.verified, 1, "only the seed matched the topic");
        assert_eq!(wide.coverage.total_matched(), 1);
        assert_eq!(wide.coverage.total, 2);

        let zero = search_with_coverage(&store, &Query::new("zephyr release", 0)).unwrap();
        assert!(zero.hits.is_empty(), "limit 0 returns no hits");
    }

    /// An unverified or contradicted lexical hit must not choose which
    /// verified neighbors enter a briefing, even when it shares a session id.
    #[test]
    fn unverified_or_contradicted_seeds_do_not_expand_verified_neighbors() {
        let counter = Entry::new("a", EntryKind::Observation, "counter evidence");
        let mut contradicted_seed = Entry::new("a", EntryKind::Decision, "amber module rollout");
        contradicted_seed.session_id = Some("session-amber".into());
        contradicted_seed.contradict(counter.id, "ops", None);
        let mut unverified_seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        unverified_seed.session_id = Some("session-harbor".into());
        let mut amber_neighbor = Entry::new("a", EntryKind::Decision, "velvet parchment");
        amber_neighbor.session_id = Some("session-amber".into());
        amber_neighbor.verify("review", None);
        let mut harbor_neighbor = Entry::new("a", EntryKind::Decision, "quartz ledger");
        harbor_neighbor.session_id = Some("session-harbor".into());
        harbor_neighbor.verify("review", None);

        let (_dir, store) = store_with(vec![
            counter,
            contradicted_seed.clone(),
            unverified_seed.clone(),
            amber_neighbor.clone(),
            harbor_neighbor.clone(),
        ]);

        let contradicted_out =
            search_with_coverage(&store, &Query::new("amber module", 10)).unwrap();
        assert!(
            contradicted_out
                .hits
                .iter()
                .any(|h| h.entry.id == contradicted_seed.id),
            "the contradicted lexical hit is visible"
        );
        assert!(
            contradicted_out
                .hits
                .iter()
                .all(|h| h.entry.id != amber_neighbor.id),
            "a contradicted seed must not expand a verified neighbor"
        );

        let unverified_out =
            search_with_coverage(&store, &Query::new("zephyr release", 10)).unwrap();
        assert!(
            unverified_out
                .hits
                .iter()
                .any(|h| h.entry.id == unverified_seed.id),
            "the unverified lexical hit is visible"
        );
        assert!(
            unverified_out
                .hits
                .iter()
                .all(|h| h.entry.id != harbor_neighbor.id),
            "an unverified seed must not expand a verified neighbor"
        );
    }

    /// Fact-shaped queries receive expansion from their temporal pool. A task
    /// cue is not required and a `FACT_CUES` word must not disable the lane.
    #[test]
    fn fact_shaped_query_still_expands_a_verified_session_neighbor() {
        let mut seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        seed.session_id = Some("session-harbor".into());
        seed.verify("review", None);
        let mut neighbor = Entry::new("a", EntryKind::Decision, "quartz ledger");
        neighbor.session_id = Some("session-harbor".into());
        neighbor.verify("review", None);
        let (_dir, store) = store_with(vec![seed.clone(), neighbor.clone()]);

        let out =
            search_with_coverage(&store, &Query::new("when was the zephyr release", 10)).unwrap();
        let seed_pos = out.hits.iter().position(|h| h.entry.id == seed.id).unwrap();
        let neighbor_pos = out
            .hits
            .iter()
            .position(|h| h.entry.id == neighbor.id)
            .unwrap();
        assert!(seed_pos < neighbor_pos, "the lexical seed stays first");
        assert_eq!(out.hits[neighbor_pos].score, 0.0);
        assert_eq!(out.coverage.total_matched(), 1);
    }

    /// A neighbor verified before an `as_of` cutoff appends from the
    /// historical pool. A cutoff before that verification leaves it out.
    #[test]
    fn as_of_cutoff_including_the_neighbor_expands_it_but_an_earlier_cutoff_does_not() {
        let verified_at = Utc::now() - chrono::Duration::days(10);
        let before = verified_at - chrono::Duration::days(1);
        let after = verified_at + chrono::Duration::days(1);

        let mut seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        seed.session_id = Some("session-harbor".into());
        seed.timestamp = before;
        seed.verify("review", None);
        seed.verification_log[0].timestamp = before;
        let mut neighbor = Entry::new("a", EntryKind::Decision, "quartz ledger");
        neighbor.session_id = Some("session-harbor".into());
        neighbor.timestamp = before;
        neighbor.verify("review", None);
        neighbor.verification_log[0].timestamp = verified_at;
        let (_dir, store) = store_with(vec![seed, neighbor.clone()]);

        let included =
            search_with_coverage(&store, &Query::new("zephyr release", 10).as_of(after)).unwrap();
        let hit = included
            .hits
            .iter()
            .find(|h| h.entry.id == neighbor.id)
            .unwrap();
        assert_eq!(hit.score, 0.0);
        assert_eq!(hit.state, StateAt::Verified);

        let excluded =
            search_with_coverage(&store, &Query::new("zephyr release", 10).as_of(before)).unwrap();
        assert!(
            excluded.hits.iter().all(|h| h.entry.id != neighbor.id),
            "an unverified neighbor at the cutoff must not expand"
        );
    }

    /// One task query can engage both append lanes. An entry eligible for
    /// the standing lane must not also appear through expansion.
    #[test]
    fn standing_and_expansion_lanes_dedupe_entries_across_both_lanes() {
        let mut seed = Entry::new("a", EntryKind::Decision, "zephyr release notes");
        seed.session_id = Some("session-harbor".into());
        seed.verify("review", None);
        let mut standing = Entry::new("a", EntryKind::Decision, "Always highlight snippets");
        standing.session_id = Some("session-harbor".into());
        standing.verify("review", None);
        let mut neighbor = Entry::new("a", EntryKind::Decision, "quartz ledger");
        neighbor.session_id = Some("session-harbor".into());
        neighbor.verify("review", None);
        let (_dir, store) = store_with(vec![seed, standing.clone(), neighbor.clone()]);

        let out = search_with_coverage(&store, &Query::new("how do i deploy zephyr", 10)).unwrap();
        let ids: Vec<_> = out.hits.iter().map(|h| h.entry.id).collect();
        let mut unique = ids.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(ids.len(), unique.len(), "no entry appears twice");
        assert_eq!(
            ids.iter().filter(|&&id| id == standing.id).count(),
            1,
            "the standing hit is not duplicated by expansion"
        );
        assert_eq!(
            ids.iter().filter(|&&id| id == neighbor.id).count(),
            1,
            "the expansion neighbor appears once"
        );
        assert!(
            out.hits.iter().any(|h| h.score > 0.0),
            "lexical hits keep their scores"
        );
    }

    #[test]
    fn answer_gate_rejects_role_modifier_mismatch() {
        let mut junior = Entry::new("a", EntryKind::Decision, "junior engineer reviews checkout");
        junior.verify("review", None);
        let mut senior = Entry::new("a", EntryKind::Decision, "senior engineer reviews checkout");
        senior.verify("review", None);
        let (_dir, store) = store_with(vec![junior.clone(), senior.clone()]);

        let asked = "which senior engineer reviews checkout";
        let ungated = search_with_coverage(&store, &Query::new(asked, 10)).unwrap();
        let ids: Vec<_> = ungated.hits.iter().map(|h| h.entry.id).collect();
        assert!(
            ids.contains(&junior.id) && ids.contains(&senior.id),
            "premise: ranking returns both candidates"
        );

        let gated =
            search_with_coverage(&store, &Query::new(asked, 10).require_evidence()).unwrap();
        assert_eq!(gated.hits.len(), 1);
        assert_eq!(gated.hits[0].entry.id, senior.id);
        assert_eq!(
            gated.coverage, ungated.coverage,
            "the gate never changes coverage"
        );
        assert!(!gated.abstained(&Query::new(asked, 10).require_evidence()));
    }

    #[test]
    fn answer_gate_rejects_numeric_mismatch() {
        let mut wrong = Entry::new("a", EntryKind::Observation, "I adopted 4 cats");
        wrong.verify("review", None);
        let mut right = Entry::new("a", EntryKind::Observation, "I adopted 3 cats");
        right.verify("review", None);
        let asked = "did I adopt 3 cats";

        let (_dir, both) = store_with(vec![wrong.clone(), right.clone()]);
        let gated = search_with_coverage(&both, &Query::new(asked, 10).require_evidence()).unwrap();
        assert_eq!(gated.hits.len(), 1, "only the 3-cat entry binds");
        assert_eq!(gated.hits[0].entry.id, right.id);

        let (_dir2, only_wrong) = store_with(vec![wrong]);
        let out =
            search_with_coverage(&only_wrong, &Query::new(asked, 10).require_evidence()).unwrap();
        assert!(out.hits.is_empty(), "4 cats cannot answer a 3-cat question");
        assert_eq!(out.coverage.verified, 1, "coverage still reports the miss");
        assert!(
            out.abstained(&Query::new(asked, 10).require_evidence()),
            "an emptied hit set is an explicit abstention"
        );
        assert!(
            !out.abstained(&Query::new(asked, 10)),
            "gate off does not abstain"
        );
    }

    #[test]
    fn answer_gate_rejects_negation_mismatch() {
        let mut positive = Entry::new("a", EntryKind::Observation, "I said I like sushi");
        positive.verify("review", None);
        let mut negative = Entry::new("a", EntryKind::Observation, "I said I do not like sushi");
        negative.verify("review", None);

        let (_dir, store) = store_with(vec![positive.clone(), negative.clone()]);
        let asked = "did I say I do not like sushi";
        let gated =
            search_with_coverage(&store, &Query::new(asked, 10).require_evidence()).unwrap();
        assert_eq!(gated.hits.len(), 1);
        assert_eq!(gated.hits[0].entry.id, negative.id);

        let (_dir2, only_positive) = store_with(vec![positive.clone()]);
        let out = search_with_coverage(&only_positive, &Query::new(asked, 10).require_evidence())
            .unwrap();
        assert!(out.hits.is_empty(), "unnegated evidence cannot answer it");

        let flat = "did I say I like sushi";
        let (_dir3, both) = store_with(vec![positive.clone(), negative.clone()]);
        let out = search_with_coverage(&both, &Query::new(flat, 10).require_evidence()).unwrap();
        assert_eq!(
            out.hits.len(),
            2,
            "a question without negation does not reject negated evidence"
        );
    }

    #[test]
    fn answer_gate_allows_positive_latest_answer_for_no_longer_update() {
        let mut current = Entry::new("a", EntryKind::Decision, "the database is postgres now");
        current.verify("review", None);
        let (_dir, store) = store_with(vec![current.clone()]);

        let q = Query::new(
            "did I replace mysql with postgres, no longer using mysql",
            10,
        )
        .require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.id, current.id);
        assert!(!out.abstained(&q));
    }

    #[test]
    fn answer_gate_allows_open_class_paraphrase() {
        let mut bought = Entry::new("a", EntryKind::Observation, "I bought a smoker for cooking");
        bought.verify("review", None);
        let (_dir, store) = store_with(vec![bought.clone()]);

        let asked = "which kitchen appliance did I buy for cooking";
        let gated =
            search_with_coverage(&store, &Query::new(asked, 10).require_evidence()).unwrap();
        assert_eq!(gated.hits.len(), 1, "no closed-class constraint to bind");
        assert_eq!(gated.hits[0].entry.id, bought.id);
    }

    #[test]
    fn answer_gate_does_not_read_bare_won_or_don_as_negation() {
        let mut award = Entry::new("a", EntryKind::Observation, "we won the latency hackathon");
        award.verify("review", None);
        let mut review = Entry::new("a", EntryKind::Decision, "Don reviewed the checkout design");
        review.verify("review", None);
        let (_dir, store) = store_with(vec![award.clone(), review.clone()]);

        for (asked, expected) in [
            ("did we won the latency hackathon", award.id),
            ("did Don review the checkout design", review.id),
        ] {
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert_eq!(out.hits.len(), 1, "{asked} must stay answerable");
            assert_eq!(out.hits[0].entry.id, expected);
            assert!(!out.abstained(&q), "{asked}");
        }
    }

    #[test]
    fn answer_gate_reads_contractions_as_negation() {
        let mut positive = Entry::new("a", EntryKind::Observation, "I deploy on fridays");
        positive.verify("review", None);
        let mut negative = Entry::new("a", EntryKind::Observation, "I don't deploy on fridays");
        negative.verify("review", None);
        let (_dir, store) = store_with(vec![positive.clone(), negative.clone()]);

        let asked = "did I say I don't deploy on fridays";
        let out = search_with_coverage(&store, &Query::new(asked, 10).require_evidence()).unwrap();
        assert_eq!(out.hits.len(), 1, "only the negated claim binds");
        assert_eq!(out.hits[0].entry.id, negative.id);

        let (_dir2, only_positive) = store_with(vec![positive]);
        let out = search_with_coverage(&only_positive, &Query::new(asked, 10).require_evidence())
            .unwrap();
        assert!(out.hits.is_empty(), "unnegated evidence cannot answer it");
    }

    #[test]
    fn answer_gate_allows_positive_answer_to_opening_negation() {
        let mut positive = Entry::new("a", EntryKind::Observation, "I deploy on fridays");
        positive.verify("review", None);
        let (_dir, store) = store_with(vec![positive.clone()]);

        for asked in ["didn't I deploy on fridays", "did I not deploy on fridays"] {
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert_eq!(out.hits.len(), 1, "{asked}");
            assert_eq!(out.hits[0].entry.id, positive.id);
            assert!(!out.abstained(&q), "{asked}");
        }
    }

    #[test]
    fn answer_gate_allows_answers_to_negated_wh_questions() {
        let mut cause = Entry::new(
            "a",
            EntryKind::Observation,
            "the checkout deploy failed because the migration timed out",
        );
        cause.verify("review", None);
        let mut tests = Entry::new(
            "a",
            EntryKind::Observation,
            "none of the flaky tests are quarantined",
        );
        tests.verify("review", None);
        let (_dir, store) = store_with(vec![cause.clone(), tests.clone()]);

        for (asked, expected) in [
            ("why didn't the checkout deploy succeed", cause.id),
            ("which flaky tests are not quarantined", tests.id),
        ] {
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert!(
                out.hits.iter().any(|hit| hit.entry.id == expected),
                "{asked}"
            );
            assert!(!out.abstained(&q), "{asked}");
        }
    }

    #[test]
    fn answer_gate_binds_padded_and_ordinal_numbers() {
        let mut padded = Entry::new("a", EntryKind::Observation, "I adopted 03 cats");
        padded.verify("review", None);
        let mut ordinal = Entry::new(
            "a",
            EntryKind::Decision,
            "sprint 3 review moved the release",
        );
        ordinal.verify("review", None);
        let (_dir, store) = store_with(vec![padded.clone(), ordinal.clone()]);

        for (asked, expected) in [
            ("did I adopt 3 cats", padded.id),
            (
                "where did the 3rd sprint review move the release",
                ordinal.id,
            ),
        ] {
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert!(
                out.hits.iter().any(|h| h.entry.id == expected),
                "{asked} must keep the entry that carries the quantity"
            );
            assert!(!out.abstained(&q), "{asked}");
        }
    }

    #[test]
    fn answer_gate_binds_number_formatting_and_ignores_relative_time() {
        let mut appliance =
            Entry::new("a", EntryKind::Observation, "I bought a smoker for cooking");
        appliance.verify("review", None);
        let mut price = Entry::new("a", EntryKind::Observation, "I paid $3,000 for the smoker");
        price.verify("review", None);
        let mut meeting = Entry::new("a", EntryKind::Observation, "the review starts at 3:00 pm");
        meeting.verify("review", None);
        let mut compact_meeting =
            Entry::new("a", EntryKind::Observation, "the review starts at 3pm");
        compact_meeting.verify("review", None);
        let mut features = Entry::new(
            "a",
            EntryKind::Decision,
            "we shipped the top 3 features in the release",
        );
        features.verify("review", None);
        let mut attendance = Entry::new(
            "a",
            EntryKind::Observation,
            "In 2024, 100 people joined the beta",
        );
        attendance.verify("review", None);
        let mut pages = Entry::new(
            "a",
            EntryKind::Observation,
            "I read pages 10-200 of the spec",
        );
        pages.verify("review", None);
        let (_dir, store) = store_with(vec![
            appliance.clone(),
            price.clone(),
            meeting.clone(),
            compact_meeting.clone(),
            features.clone(),
            attendance.clone(),
            pages.clone(),
        ]);

        for (asked, expected) in [
            ("what did I buy for cooking 3 days ago", appliance.id),
            ("did I pay 3000 dollars for the smoker", price.id),
            ("did the review start at 3pm", meeting.id),
            ("did the review start at 3:00pm", compact_meeting.id),
            ("did we ship 3 features in the release", features.id),
            ("did 100 people join the beta in 2024", attendance.id),
            ("did I read pages 10 to 200 of the spec", pages.id),
        ] {
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert!(
                out.hits.iter().any(|hit| hit.entry.id == expected),
                "{asked}"
            );
            assert!(!out.abstained(&q), "{asked}");
        }

        let q = Query::new("did the review start at 4pm", 10).require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        assert!(out.hits.is_empty());
        assert!(out.abstained(&q));
    }

    #[test]
    fn answer_gate_collects_plural_role_questions() {
        for (asked, summary) in [
            (
                "which managers own the checkout cache",
                "manager owns the checkout cache",
            ),
            (
                "which leads own the checkout cache",
                "lead owns the checkout cache",
            ),
        ] {
            let mut entry = Entry::new("a", EntryKind::Decision, summary);
            entry.verify("review", None);
            let (_dir, store) = store_with(vec![entry.clone()]);
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert_eq!(out.hits.len(), 1, "{asked}");
            assert_eq!(out.hits[0].entry.id, entry.id);
            assert!(!out.abstained(&q), "{asked}");
        }
    }

    #[test]
    fn answer_gate_binds_plural_role_evidence() {
        for (asked, summary) in [
            (
                "which manager of engineering owns the checkout cache",
                "managers own the checkout cache",
            ),
            (
                "which lead engineer owns the checkout cache",
                "the leads own the checkout cache",
            ),
            (
                "which staff engineer reviews checkout",
                "the staffs review checkout",
            ),
            (
                "which associate engineer reviews checkout",
                "the associates review checkout",
            ),
            (
                "which distinguished engineer reviews checkout",
                "the distinguished engineer reviews checkout",
            ),
        ] {
            let mut entry = Entry::new("a", EntryKind::Decision, summary);
            entry.verify("review", None);
            let (_dir, store) = store_with(vec![entry.clone()]);
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert_eq!(out.hits.len(), 1, "{asked}");
            assert_eq!(out.hits[0].entry.id, entry.id);
            assert!(!out.abstained(&q), "{asked}");
        }
    }

    #[test]
    fn answer_gate_ignores_title_words_outside_role_context() {
        let mut package = Entry::new(
            "a",
            EntryKind::Observation,
            "the frontend installs dependencies with pnpm",
        );
        package.verify("review", None);
        let mut advice = Entry::new("a", EntryKind::Decision, "use redis for checkout caching");
        advice.verify("review", None);
        let mut timing = Entry::new(
            "a",
            EntryKind::Observation,
            "checkout releases ship two weeks after code freeze",
        );
        timing.verify("review", None);
        let mut leader = Entry::new(
            "a",
            EntryKind::Decision,
            "the senior engineer led the checkout migration",
        );
        leader.verify("review", None);
        let (_dir, store) = store_with(vec![
            package.clone(),
            advice.clone(),
            timing.clone(),
            leader.clone(),
        ]);

        for (asked, expected) in [
            ("which package manager does the frontend use", package.id),
            ("what did the assistant recommend for caching", advice.id),
            (
                "what did the assistant tell the developer about caching",
                advice.id,
            ),
            ("what is the lead time for checkout releases", timing.id),
            (
                "did the senior engineer lead the checkout migration",
                leader.id,
            ),
        ] {
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert!(
                out.hits.iter().any(|hit| hit.entry.id == expected),
                "{asked}"
            );
            assert!(!out.abstained(&q), "{asked}");
        }
    }

    #[test]
    fn answer_gate_rejects_verb_instead_of_role_modifier() {
        let mut junior = Entry::new(
            "a",
            EntryKind::Decision,
            "junior engineer leads checkout reviews",
        );
        junior.verify("review", None);
        let (_dir, store) = store_with(vec![junior]);

        let q = Query::new("which lead engineer reviews checkout", 10).require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        assert!(out.hits.is_empty());
        assert!(out.abstained(&q));
    }

    #[test]
    fn answer_gate_keeps_digit_bearing_identifiers_exact() {
        let mut build = Entry::new("a", EntryKind::Decision, "build 001abc moved the release");
        build.verify("review", None);
        let (_dir, store) = store_with(vec![build]);

        let q = Query::new("which build 1abc moved the release", 10).require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        assert!(out.hits.is_empty());
        assert!(out.abstained(&q));
    }

    #[test]
    fn answer_gate_does_not_treat_verb_inflections_as_title_modifiers() {
        let mut owner = Entry::new(
            "a",
            EntryKind::Decision,
            "Priya owns the checkout migration",
        );
        owner.verify("review", None);
        let (_dir, store) = store_with(vec![owner.clone()]);

        let q = Query::new("who leads the checkout migration", 10).require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.id, owner.id);
        assert!(!out.abstained(&q));
    }

    #[test]
    fn answer_gate_leaves_verified_provenance_unchanged() {
        let mut verified_near_miss =
            Entry::new("a", EntryKind::Decision, "junior engineer reviews checkout");
        verified_near_miss.verify("review", None);
        let unverified_answer =
            Entry::new("a", EntryKind::Decision, "senior engineer reviews billing");
        let (_dir, store) = store_with(vec![verified_near_miss.clone(), unverified_answer.clone()]);

        let asked = "which senior engineer reviews";
        let provenance_query = Query::new(asked, 10).require_verified();
        let provenance = search_with_coverage(&store, &provenance_query).unwrap();
        assert_eq!(provenance.hits.len(), 1);
        assert_eq!(provenance.hits[0].entry.id, verified_near_miss.id);
        assert!(!provenance.abstained(&provenance_query));

        let combined_query = Query::new(asked, 10).require_verified().require_evidence();
        let combined = search_with_coverage(&store, &combined_query).unwrap();
        assert_eq!(combined.coverage, provenance.coverage);
        assert!(combined.hits.is_empty());
        assert!(combined.abstained(&combined_query));
    }

    #[test]
    fn answer_gate_abstains_on_mismatch_and_missing_evidence() {
        let mut junior = Entry::new("a", EntryKind::Decision, "junior engineer reviews checkout");
        junior.verify("review", None);
        let (_dir, store) = store_with(vec![junior]);

        let mismatch = Query::new("which senior engineer reviews checkout", 10).require_evidence();
        let out = search_with_coverage(&store, &mismatch).unwrap();
        assert!(out.coverage.verified > 0, "knowledge exists on the topic");
        assert!(
            out.abstained(&mismatch),
            "matched knowledge that fails the constraints is a refusal"
        );

        let missing = Query::new("which principal astronaut pilots zephyr", 10).require_evidence();
        let out = search_with_coverage(&store, &missing).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage.total_matched(), 0);
        assert!(out.abstained(&missing));

        let answerable =
            Query::new("which junior engineer reviews checkout", 10).require_evidence();
        let out = search_with_coverage(&store, &answerable).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert!(
            !out.abstained(&answerable),
            "an answerable question answers"
        );
    }

    #[test]
    fn answer_gate_limit_zero_does_not_abstain() {
        let mut junior = Entry::new("a", EntryKind::Decision, "junior engineer reviews checkout");
        junior.verify("review", None);
        let (_dir, store) = store_with(vec![junior]);

        let zero = Query::new("which senior engineer reviews checkout", 0).require_evidence();
        let out = search_with_coverage(&store, &zero).unwrap();
        assert!(out.hits.is_empty());
        assert!(
            !out.abstained(&zero),
            "a zero-limit page is the caller's choice, not a miss"
        );
    }

    #[test]
    fn answer_gate_without_constraints_leaves_results_identical() {
        let mut junior = Entry::new("a", EntryKind::Decision, "junior engineer reviews checkout");
        junior.verify("review", None);
        let mut senior = Entry::new("a", EntryKind::Decision, "senior engineer reviews checkout");
        senior.verify("review", None);
        let (_dir, store) = store_with(vec![junior, senior]);

        let asked = "who reviews checkout";
        let plain = search_with_coverage(&store, &Query::new(asked, 10)).unwrap();
        let gated_query = Query::new(asked, 10).require_evidence();
        let gated = search_with_coverage(&store, &gated_query).unwrap();
        let shape = |out: &SearchOutcome| {
            out.hits
                .iter()
                .map(|hit| (hit.entry.id, hit.score))
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&gated), shape(&plain));
        assert_eq!(gated.coverage, plain.coverage);
        assert!(!gated.abstained(&gated_query));
    }

    #[test]
    fn answer_gate_binds_complete_numeric_groups_across_topical_entries() {
        let mut quiz = Entry::new(
            "a",
            EntryKind::Observation,
            "my quiz score improved from 65% to 82% after classifying triangles by sides",
        );
        quiz.verify("review", None);
        let mut test = Entry::new(
            "a",
            EntryKind::Observation,
            "my test score improved from 80% to 92% on congruence proofs and similarity calculations",
        );
        test.verify("review", None);
        let mut travel = Entry::new(
            "a",
            EntryKind::Observation,
            "my travel journal lists 65 cities and 82 countries",
        );
        travel.verify("review", None);
        let (_dir, store) = store_with(vec![test.clone(), quiz.clone(), travel.clone()]);
        let asked = "Which improvement happened first: my quiz score increasing from 65% to 82% after focusing on triangle side classifications, or my test score rising from 80% to 92% on congruence proofs and similarity calculations?";
        let plain = search_with_coverage(&store, &Query::new(asked, 10)).unwrap();
        assert!(
            [test.id, quiz.id, travel.id]
                .iter()
                .all(|id| plain.hits.iter().any(|hit| hit.entry.id == *id)),
            "premise: ranking returns both witnesses and the unrelated numeric pair"
        );

        let q = Query::new(asked, 10).require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        let expected: Vec<_> = plain
            .hits
            .iter()
            .filter(|hit| hit.entry.id == test.id || hit.entry.id == quiz.id)
            .collect();
        let actual: Vec<_> = out.hits.iter().collect();
        assert_eq!(actual.len(), 2);
        assert_eq!(actual[0].entry.id, expected[0].entry.id);
        assert_eq!(actual[1].entry.id, expected[1].entry.id);
        assert_eq!(actual[0].score, expected[0].score);
        assert_eq!(actual[1].score, expected[1].score);
        assert!(out.hits.iter().all(|hit| hit.entry.id != travel.id));
        assert_eq!(out.coverage, plain.coverage);
        assert!(!out.abstained(&q));
    }

    #[test]
    fn answer_gate_set_binding_requires_complete_topical_groups() {
        let asked = "Which improvement happened first: my quiz score increasing from 65% to 82% after focusing on triangle side classifications, or my test score rising from 80% to 92% on congruence proofs and similarity calculations?";
        let mut quiz = Entry::new(
            "a",
            EntryKind::Observation,
            "my quiz score improved from 65% to 82% after classifying triangles by sides",
        );
        quiz.verify("review", None);
        let mut travel = Entry::new(
            "a",
            EntryKind::Observation,
            "my travel journal lists 80 cities and 92 countries",
        );
        travel.verify("review", None);
        let mut partial_test = Entry::new(
            "a",
            EntryKind::Observation,
            "my test score reached 80% on congruence proofs",
        );
        partial_test.verify("review", None);
        let mut partial_similarity = Entry::new(
            "a",
            EntryKind::Observation,
            "my test score reached 92% on similarity calculations",
        );
        partial_similarity.verify("review", None);

        for entries in [
            vec![quiz.clone(), travel],
            vec![quiz.clone(), partial_test, partial_similarity],
            vec![quiz],
        ] {
            let (_dir, store) = store_with(entries);
            let q = Query::new(asked, 10).require_evidence();
            let out = search_with_coverage(&store, &q).unwrap();
            assert!(out.hits.is_empty());
            assert!(out.abstained(&q));
        }
    }

    #[test]
    fn answer_gate_set_binding_requires_a_complete_returned_page() {
        let mut quiz = Entry::new(
            "a",
            EntryKind::Observation,
            "my quiz score improved from 65% to 82% after classifying triangles by sides",
        );
        quiz.verify("review", None);
        let mut test = Entry::new(
            "a",
            EntryKind::Observation,
            "my test score improved from 80% to 92% on congruence proofs and similarity calculations",
        );
        test.verify("review", None);
        let asked = "Which improvement happened first: my quiz score increasing from 65% to 82% after focusing on triangle side classifications, or my test score rising from 80% to 92% on congruence proofs and similarity calculations?";

        let (_dir, store) = store_with(vec![quiz, test]);
        let q = Query::new(asked, 1).require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        assert!(out.hits.is_empty());
        assert!(out.abstained(&q));

        let zero = Query::new(asked, 0).require_evidence();
        let out = search_with_coverage(&store, &zero).unwrap();
        assert!(out.hits.is_empty());
        assert!(!out.abstained(&zero));
    }

    #[test]
    fn answer_gate_set_binding_keeps_verified_provenance() {
        let mut quiz = Entry::new(
            "a",
            EntryKind::Observation,
            "my quiz score improved from 65% to 82% after classifying triangles by sides",
        );
        quiz.verify("review", None);
        let test = Entry::new(
            "a",
            EntryKind::Observation,
            "my test score improved from 80% to 92% on congruence proofs and similarity calculations",
        );
        let (_dir, store) = store_with(vec![quiz, test]);
        let asked = "Which improvement happened first: my quiz score increasing from 65% to 82% after focusing on triangle side classifications, or my test score rising from 80% to 92% on congruence proofs and similarity calculations?";
        let q = Query::new(asked, 10).require_verified().require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage.verified, 1);
        assert_eq!(out.coverage.unverified, 1);
        assert!(out.abstained(&q));
    }

    #[test]
    fn answer_gate_ambiguous_numeric_groups_keep_per_entry_abstention() {
        let mut three = Entry::new("a", EntryKind::Observation, "I scored 3 on the quiz");
        three.verify("review", None);
        let mut four = Entry::new("a", EntryKind::Observation, "I scored 4 on the quiz");
        four.verify("review", None);
        let (_dir, store) = store_with(vec![three, four]);
        let q = Query::new("did I score 3 or 4 on the quiz", 10).require_evidence();
        let out = search_with_coverage(&store, &q).unwrap();
        assert!(out.hits.is_empty());
        assert!(out.abstained(&q));
    }

    #[test]
    fn answer_gate_excludes_standing_lane() {
        let mut seed = Entry::new("a", EntryKind::Decision, "write zephyr briefs carefully");
        seed.verify("review", None);
        let mut standing = Entry::new("a", EntryKind::Decision, "Always highlight snippets");
        standing.verify("review", None);
        let (_dir, store) = store_with(vec![seed.clone(), standing.clone()]);

        let asked = "how do i write zephyr briefs";
        let wide = search_with_coverage(&store, &Query::new(asked, 10)).unwrap();
        assert!(wide.hits.iter().any(|hit| hit.entry.id == standing.id));

        let q = Query::new(asked, 10).require_evidence();
        let gated = search_with_coverage(&store, &q).unwrap();
        assert_eq!(gated.hits.len(), 1);
        assert_eq!(gated.hits[0].entry.id, seed.id);
    }

    #[test]
    fn answer_gate_excludes_expansion_lane() {
        let mut seed = Entry::new("a", EntryKind::Observation, "I said I like sushi");
        seed.session_id = Some("session-harbor".into());
        seed.verify("review", None);
        let mut neighbor = Entry::new("a", EntryKind::Observation, "I never eat after 9pm");
        neighbor.session_id = Some("session-harbor".into());
        neighbor.verify("review", None);
        let (_dir, store) = store_with(vec![seed.clone(), neighbor.clone()]);

        let asked = "did I say I do not like sushi";
        let wide = search_with_coverage(&store, &Query::new(asked, 10)).unwrap();
        assert!(wide.hits.iter().any(|hit| hit.entry.id == neighbor.id));

        let q = Query::new(asked, 10).require_evidence();
        let gated = search_with_coverage(&store, &q).unwrap();
        assert!(gated.hits.is_empty());
        assert!(gated.abstained(&q));
    }
}
