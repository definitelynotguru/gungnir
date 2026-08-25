//! Keyword recall: token overlap scored over summary and body, ordered by
//! verification bucket first so verified facts outrank hearsay and
//! contradicted facts sink. Rolled-back entries are hidden by default.

use crate::entry::{Entry, VerificationState};
use crate::{Result, Store};

#[derive(Clone, Debug)]
pub struct Query {
    pub text: String,
    pub limit: usize,
}

impl Query {
    pub fn new(text: impl Into<String>, limit: usize) -> Self {
        Self {
            text: text.into(),
            limit,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub entry: Entry,
    pub score: f64,
}

/// Rank bucket: higher sorts first.
pub fn bucket(entry: &Entry) -> u8 {
    match entry.verification {
        VerificationState::Verified => 3,
        VerificationState::Unverified => 2,
        VerificationState::Contradicted { .. } => 1,
        VerificationState::RolledBack => 0,
    }
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

/// Search `store`, best hits first. Entries with zero overlap are dropped.
pub fn search(store: &Store, query: &Query) -> Result<Vec<Hit>> {
    let qtokens = tokenize(&query.text);
    let mut qtokens_sorted = qtokens.clone();
    qtokens_sorted.sort();
    qtokens_sorted.dedup();

    let mut hits: Vec<Hit> = Vec::new();
    for entry in store.entries()? {
        if entry.verification == VerificationState::RolledBack {
            continue;
        }
        let s = score(&entry, &qtokens_sorted);
        if s > 0.0 {
            hits.push(Hit { entry, score: s });
        }
    }
    hits.sort_by(|a, b| {
        bucket(&b.entry)
            .cmp(&bucket(&a.entry))
            .then(b.score.total_cmp(&a.score))
            .then(a.entry.id.cmp(&b.entry.id))
    });
    hits.truncate(query.limit);
    Ok(hits)
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

        let hits = search(&store, &Query::new("postgres queue", 10)).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].entry.verification, VerificationState::Verified);
    }

    #[test]
    fn rolled_back_entries_are_hidden() {
        let mut e = Entry::new("a", EntryKind::Decision, "postgres pooling");
        e.mark_rolled_back("rb");
        let (_d, store) = store_with(vec![e]);
        assert!(search(&store, &Query::new("postgres", 10))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn summary_hits_outweigh_body_hits() {
        let mut in_summary = Entry::new("a", EntryKind::Decision, "migrating off redis");
        in_summary.body = "unrelated".into();
        let mut in_body = Entry::new("a", EntryKind::Observation, "infra note");
        in_body.body = "we are migrating off redis soon".into();
        let (_d, store) = store_with(vec![in_summary, in_body]);

        let hits = search(&store, &Query::new("migrating redis", 10)).unwrap();
        assert!(hits[0].score > hits[1].score);
        assert_eq!(hits[0].entry.summary, "migrating off redis");
    }

    #[test]
    fn no_overlap_yields_no_hits() {
        let e = Entry::new("a", EntryKind::Decision, "kubernetes ingress");
        let (_d, store) = store_with(vec![e]);
        assert!(search(&store, &Query::new("quantum entanglement", 5))
            .unwrap()
            .is_empty());
    }
}
