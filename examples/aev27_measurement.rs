//! AEV-27 targeted before/after answer-binding measurement.
//!
//! Reproduces the failure AEV-27 named without the full LME/BEAM eval: each
//! case stores one verified entry that shares lexical terms with the
//! question, so retrieval succeeds and provenance is satisfied. What fails
//! is *binding* — the returned evidence does not carry the role word, the
//! number, or the negation the question asked about, and abstention used to
//! test only "did a verified hit exist".
//!
//! `cargo run --example aev27_measurement` prints, per case, the hit count
//! with the gate off and with `require_evidence` on, plus the exact query
//! and evidence text. `unanswerable_empty` counts the cases where a hit
//! exists before the gate and none after it; `answerable_empty` is the
//! control leg and must stay zero on both sides. Both totals are empty-result
//! counts, not judged-answer accuracy, and this is not a benchmark baseline.
//! `src/recall.rs` pins the behavior the fixed read path provides.

use gungnir::recall::{search_with_coverage, Query};
use gungnir::{Entry, EntryKind, Store};

enum Leg {
    Unanswerable,
    Answerable,
}

struct Case {
    leg: Leg,
    query: &'static str,
    summary: &'static str,
}

fn cases() -> Vec<Case> {
    let unanswerable = [
        (
            "which Software Engineer Manager evaluates the checkout pipeline",
            "Senior Software Engineer evaluates the checkout pipeline",
        ),
        ("did I adopt 3 cats", "I adopted 4 cats"),
        ("did I say I do not like sushi", "I said I like sushi"),
    ];
    let answerable = [
        (
            "which Software Engineer Manager evaluates the checkout pipeline",
            "Software Engineer Manager evaluates the checkout pipeline",
        ),
        ("did I adopt 3 cats", "I adopted 3 cats"),
        (
            "did I say I do not like sushi",
            "I said I do not like sushi",
        ),
        (
            "which kitchen appliance did I buy for cooking",
            "I bought a smoker for cooking",
        ),
    ];
    unanswerable
        .into_iter()
        .map(|(query, summary)| Case {
            leg: Leg::Unanswerable,
            query,
            summary,
        })
        .chain(answerable.into_iter().map(|(query, summary)| Case {
            leg: Leg::Answerable,
            query,
            summary,
        }))
        .collect()
}

fn main() {
    let mut unanswerable_empty = (0usize, 0usize);
    let mut answerable_empty = (0usize, 0usize);
    let mut unanswerable_total = 0usize;
    let mut answerable_total = 0usize;
    let mut lines = Vec::new();

    for c in cases() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut entry = Entry::new("a", EntryKind::Observation, c.summary);
        entry.verify("archive", None);
        store.create(&entry).unwrap();

        let off = search_with_coverage(&store, &Query::new(c.query, 10)).unwrap();
        let on = search_with_coverage(&store, &Query::new(c.query, 10).require_evidence()).unwrap();

        let leg = match c.leg {
            Leg::Unanswerable => {
                unanswerable_total += 1;
                unanswerable_empty.0 += usize::from(off.hits.is_empty());
                unanswerable_empty.1 += usize::from(on.hits.is_empty());
                "unanswerable"
            }
            Leg::Answerable => {
                answerable_total += 1;
                answerable_empty.0 += usize::from(off.hits.is_empty());
                answerable_empty.1 += usize::from(on.hits.is_empty());
                "answerable  "
            }
        };
        lines.push(format!(
            "  {leg} before_empty={:<5} after_empty={:<5} off_hits={} verified={} on_hits={} query={:?}\n      evidence={:?}",
            off.hits.is_empty(),
            on.hits.is_empty(),
            off.hits.len(),
            off.coverage.verified,
            on.hits.len(),
            c.query,
            c.summary,
        ));
    }

    for line in &lines {
        println!("{line}");
    }
    println!(
        "unanswerable_empty before={}/{} after={}/{}",
        unanswerable_empty.0, unanswerable_total, unanswerable_empty.1, unanswerable_total
    );
    println!(
        "answerable_empty before={}/{} after={}/{}",
        answerable_empty.0, answerable_total, answerable_empty.1, answerable_total
    );
}
