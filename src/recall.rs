//! Keyword recall: token overlap scored over summary and body, ordered by
//! verification bucket first so verified facts outrank hearsay and
//! contradicted facts sink. Rolled-back entries are hidden by default.
//!
//! Temporal modes:
//! - `as_of` evaluates candidate entries as they existed at an instant,
//!   derived from verification-log timestamps. No schema beyond what is
//!   already written.
//! - `current_only` resolves revises chains to their heads and drops
//!   contradicted facts, answering "what do we believe now".

use chrono::{DateTime, Utc};

use crate::entry::{Entry, VerificationState};
use crate::{Result, Store};

#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub limit: usize,
    /// Evaluate candidates as of this instant instead of now.
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
}

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "in",
    "is", "it", "its", "of", "on", "or", "that", "the", "this", "to", "was", "were", "will",
    "with",
];

pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(t))
        .map(str::to_owned)
        .collect()
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
enum StateAt {
    Verified,
    Unverified,
    Contradicted,
    RolledBack,
}

fn state_at(entry: &Entry, as_of: Option<DateTime<Utc>>) -> StateAt {
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
        .map(|r| match r.status.as_str() {
            "verified" => StateAt::Verified,
            "contradicted" => StateAt::Contradicted,
            "rolled_back" => StateAt::RolledBack,
            _ => StateAt::Unverified,
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

/// Weighted overlap: a query token hit in the summary counts double versus
/// the body, normalized by unique query token count. 0.0 means no signal.
pub fn score(entry: &Entry, query_tokens: &[String]) -> f64 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let summary = tokenize(&entry.summary);
    let body = tokenize(&entry.body);
    let mut raw = 0.0;
    for qt in query_tokens {
        if summary.iter().any(|t| t == qt) {
            raw += 2.0;
        } else if body.iter().any(|t| t == qt) {
            raw += 1.0;
        }
    }
    raw / query_tokens.len() as f64
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

    let mut matched: Vec<(Entry, f64, StateAt)> = Vec::new();
    let mut coverage = Coverage::default();
    // Collect revises targets from every as-of-valid entry, not just
    // topic-matched ones. Otherwise a chain A<-B<-C where B is off-topic
    // leaves A visible next to C.
    let mut revised: std::collections::HashSet<EntryId> = std::collections::HashSet::new();

    for entry in store.entries()? {
        if let Some(cutoff) = query.as_of {
            if entry.timestamp > cutoff {
                continue;
            }
        }
        if query.current_only {
            if let Some(prev) = entry.revises {
                revised.insert(prev);
            }
        }
        let s = score(&entry, &qtokens);
        if s <= 0.0 {
            continue;
        }
        let state = state_at(&entry, query.as_of);
        match state {
            StateAt::RolledBack => coverage.hidden_rolled_back += 1,
            _ => matched.push((entry, s, state)),
        }
    }

    if query.current_only {
        let before = matched.len();
        matched.retain(|(e, _, _)| !revised.contains(&e.id));
        coverage.hidden_superseded += before - matched.len();
        matched.retain(|(_, _, st)| {
            if *st == StateAt::Contradicted {
                coverage.contradicted += 1;
                false
            } else {
                true
            }
        });
    }

    for (_, _, st) in &matched {
        match st {
            StateAt::Verified => coverage.verified += 1,
            StateAt::Unverified => coverage.unverified += 1,
            StateAt::Contradicted => coverage.contradicted += 1,
            StateAt::RolledBack => unreachable!("rolled back filtered above"),
        }
    }

    matched.sort_by(|a, b| {
        let (ea, sa, _) = a;
        let (eb, sb, _) = b;
        bucket_at(eb, query.as_of)
            .cmp(&bucket_at(ea, query.as_of))
            .then(sb.total_cmp(sa))
            .then(ea.id.cmp(&eb.id))
    });

    let hits = matched
        .into_iter()
        .take(query.limit)
        .map(|(entry, score, _)| Hit { entry, score })
        .collect();

    Ok(SearchOutcome { hits, coverage })
}

/// Search, best hits first. Entries with zero overlap are dropped.
pub fn search(store: &Store, query: &Query) -> Result<Vec<Hit>> {
    search_with_coverage(store, query).map(|o| o.hits)
}

use crate::id::EntryId;

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
            vec!["checkout", "query", "slow"]
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
    fn rolled_back_entries_are_hidden() {
        let mut e = Entry::new("a", EntryKind::Decision, "postgres pooling");
        e.mark_rolled_back("rb");
        let (_d, store) = store_with(vec![e]);
        let out = search_with_coverage(&store, &Query::new("postgres", 10)).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage.hidden_rolled_back, 1);
    }

    #[test]
    fn no_overlap_yields_no_hits_and_zero_coverage() {
        let e = Entry::new("a", EntryKind::Decision, "kubernetes ingress");
        let (_d, store) = store_with(vec![e]);
        let out = search_with_coverage(&store, &Query::new("quantum entanglement", 5)).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage, Coverage::default());
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
}
