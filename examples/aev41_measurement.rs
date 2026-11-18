//! AEV-41 targeted in-text date retrieval measurement.
//!
//! Run with `cargo run --example aev41_measurement`.
//! The four recoverable cases have no stemmed topic-word overlap between the
//! question and the entry. The baseline is 0/4 because BM25 cannot equate an
//! absolute date with a relative expression. The fixed expectation is 4/4.
//! Two controls pin empty retrieval on a wrong date and BM25 fallback when a
//! parsed query date has no index match. This is not a full LME or BEAM eval.

use chrono::{Duration, TimeZone, Utc};
use gungnir::recall::Query;
use gungnir::{Entry, EntryKind, Store};

struct Case {
    name: &'static str,
    query: String,
    entry_summary: &'static str,
    entry_body: String,
}

fn cases(question_time: chrono::DateTime<Utc>) -> Vec<Case> {
    let iso = |days_ago| {
        (question_time - Duration::days(days_ago))
            .date_naive()
            .to_string()
    };
    vec![
        Case {
            name: "kitchen appliance, relative query",
            query: "Which kitchen appliance did I buy 10 days ago?".into(),
            entry_summary: "smoker purchase",
            entry_body: format!("Bought a smoker on {}.", iso(10)),
        },
        Case {
            name: "kitchen appliance, relative entry",
            query: format!("Which item is dated {}?", iso(10)),
            entry_summary: "smoker purchase",
            entry_body: "I bought a smoker ten days ago.".into(),
        },
        Case {
            name: "four weeks, relative query",
            query: "What did I purchase four weeks ago?".into(),
            entry_summary: "espresso machine order",
            entry_body: format!("Ordered on {}.", iso(28)),
        },
        Case {
            name: "four weeks, relative entry",
            query: format!("Which object is dated {}?", iso(28)),
            entry_summary: "espresso machine order",
            entry_body: "I ordered the espresso machine four weeks ago.".into(),
        },
    ]
}

fn verified_entry(case: &Case, question_time: chrono::DateTime<Utc>) -> Entry {
    let mut entry = Entry::new("eval", EntryKind::Session, case.entry_summary);
    entry.timestamp = question_time;
    entry.body = case.entry_body.clone();
    entry.verify("archive", None);
    entry
}

fn main() {
    let question_time = Utc.with_ymd_and_hms(2026, 11, 18, 12, 0, 0).unwrap();
    let cases = cases(question_time);
    let total = cases.len();
    let mut recovered = 0;

    for case in &cases {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let entry = verified_entry(case, question_time);
        store.create(&entry).unwrap();

        let out = gungnir::recall::search_with_coverage(
            &store,
            &Query::new(&case.query, 10).question_time(question_time),
        )
        .unwrap();
        let hit = out.hits.iter().any(|hit| hit.entry.id == entry.id);
        recovered += usize::from(hit);
        println!("{:<38} recovered={hit}", case.name);
    }

    let wrong_date = {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let case = Case {
            name: "wrong date control",
            query: "Which kitchen appliance did I buy a week ago?".into(),
            entry_summary: "smoker purchase",
            entry_body: format!(
                "Bought a smoker on {}.",
                (question_time - Duration::days(10)).date_naive()
            ),
        };
        store.create(&verified_entry(&case, question_time)).unwrap();
        gungnir::recall::search_with_coverage(
            &store,
            &Query::new(&case.query, 10).question_time(question_time),
        )
        .unwrap()
        .hits
        .is_empty()
    };

    let lexical_fallback = {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let case = Case {
            name: "lexical fallback control",
            query: "smoker purchase a week ago".into(),
            entry_summary: "smoker purchase",
            entry_body: format!(
                "Bought a smoker on {}.",
                (question_time - Duration::days(10)).date_naive()
            ),
        };
        let entry = verified_entry(&case, question_time);
        store.create(&entry).unwrap();
        gungnir::recall::search_with_coverage(
            &store,
            &Query::new(&case.query, 10).question_time(question_time),
        )
        .unwrap()
        .hits
        .iter()
        .any(|hit| hit.entry.id == entry.id)
    };

    println!("date_recovered={recovered}/{total}");
    println!("wrong_date_empty={}/1", usize::from(wrong_date));
    println!("lexical_fallback={}/1", usize::from(lexical_fallback));
}
