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

use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};

use crate::entry::{Entry, EntryKind, RecordStatus, VerificationState};
use crate::id::EntryId;
use crate::store::Skipped;
use crate::{Result, Store};

#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub limit: usize,
    /// Evaluate candidates as they existed at an instant instead of now.
    pub as_of: Option<DateTime<Utc>>,
    /// Keep only revises-chain heads and drop contradicted facts.
    pub current_only: bool,
}

impl Query {
    pub fn new(text: impl Into<String>, limit: usize) -> Self {
        Self {
            text: text.into(),
            limit,
            as_of: None,
            current_only: false,
        }
    }

    pub fn as_of(mut self, at: DateTime<Utc>) -> Self {
        self.as_of = Some(at);
        self
    }

    pub fn current(mut self) -> Self {
        self.current_only = true;
        self
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub entry: Entry,
    pub score: f64,
}

/// Topic-scoped counts behind a result set. Powers the abstention signal:
/// "no verified knowledge" is a claim about coverage, not just empty output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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
}

#[derive(Clone, Debug)]
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    pub coverage: Coverage,
    /// ULID-named files that failed to parse and were skipped.
    pub skipped: Vec<Skipped>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateAt {
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

/// The candidate pool after `as_of` filtering, plus the set of ids hidden
/// by `current_only` supersession. Shared by keyword and hybrid search so
/// both paths honor the temporal contract identically.
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
        // A rolled-back head must not hide its predecessor. Current view
        // falls back to the last live revision. A rollback record's
        // `revises` marks the ancestor it restores, so it is not a revision.
        if query.current_only && state != StateAt::RolledBack && entry.kind != EntryKind::Rollback {
            if let Some(prev) = entry.revises {
                revised.insert(prev);
            }
        }
        pool.push(Candidate { entry, state });
    }
    Ok((pool, revised, scan.skipped))
}

/// Shared tail of every search: coverage accounting, trust×recency
/// weighting, bucket-first ordering, truncation.
///
/// `matched` holds topic-matched candidates with their raw relevance
/// (BM25-lite or RRF). Coverage counts run over that pool before
/// truncation, so callers can say "no verified knowledge covers this task"
/// even when hits exist.
pub(crate) fn finish(
    mut matched: Vec<(Entry, StateAt, f64)>,
    revised: &HashSet<EntryId>,
    query: &Query,
    total: usize,
    skipped: Vec<Skipped>,
) -> SearchOutcome {
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

    let now = query.as_of.unwrap_or_else(Utc::now);
    let mut scored: Vec<(Entry, StateAt, f64)> = matched
        .into_iter()
        .map(|(e, st, rel)| {
            let final_score = rel * trust_weight(st) * recency_weight(&e, now);
            (e, st, final_score)
        })
        .collect();

    scored.sort_by(|a, b| {
        bucket_at(&b.0, query.as_of)
            .cmp(&bucket_at(&a.0, query.as_of))
            .then(b.2.total_cmp(&a.2))
            .then(a.0.id.cmp(&b.0.id))
    });

    let hits = scored
        .into_iter()
        .take(query.limit)
        .map(|(entry, _, score)| Hit { entry, score })
        .collect();

    SearchOutcome {
        hits,
        coverage,
        skipped,
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
        .into_iter()
        .filter_map(|c| {
            let s = score(&c.entry, &qtokens, &stats);
            (s > 0.0).then_some((c.entry, c.state, s))
        })
        .collect();

    Ok(finish(matched, &revised, query, total, skipped))
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
}
