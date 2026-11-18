//! AEV-43 targeted before/after measurement.
//!
//! Reproduces AEV-25's stale>latest mechanism: a stale verified chain tail
//! and a newer live verified head share an explicit `revises` slot, both
//! match the query, and the stale wording carries more query tokens. A plain
//! present-time query (no `--current`) ranks the stale tail above the head
//! unless recall resolves the chain as a slot on the read path.
//!
//! Each head shares two stemmed frame tokens with its tail, so a `revises`
//! link between them is a realistic explicit resolution rather than a
//! divergent-wording pair that would never have been linked.
//!
//! Run on any revision: `cargo run --example aev43_measurement`.
//! Prints `stale_first=<n>/<N>` and `head_absent=<n>/<N>`. The fixed read
//! path drives stale_first to 0/N. No assertion here: the same command
//! measures before and after, and `src/recall.rs` pins the fixed behavior.

use chrono::{Duration, Utc};
use gungnir::recall::Query;
use gungnir::{Entry, EntryKind, Store};

struct Case {
    query: &'static str,
    stale_summary: &'static str,
    stale_body: &'static str,
    head_summary: &'static str,
    head_body: &'static str,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            query: "how many commits merged into main",
            stale_summary: "12 commits merged into main",
            stale_body: "12 commits merged into main total count",
            head_summary: "main now has 19 commits",
            head_body: "merge count updated",
        },
        Case {
            query: "deadline for first sprint",
            stale_summary: "deadline for first sprint is May 3",
            stale_body: "deadline first sprint May 3 deadline",
            head_summary: "sprint deadline pushed to June 14",
            head_body: "rescheduled the deadline",
        },
        Case {
            query: "which database do sessions use",
            stale_summary: "sessions use mysql database",
            stale_body: "sessions use the mysql database for storage",
            head_summary: "sessions use postgres now",
            head_body: "cutover finished",
        },
        Case {
            query: "what is the deploy window time",
            stale_summary: "deploy window is 2am time",
            stale_body: "deploy window 2am time deploy window",
            head_summary: "deploy window moved to 4am",
            head_body: "new schedule approved",
        },
        Case {
            query: "price of the pro plan",
            stale_summary: "pro plan price is 20 dollars",
            stale_body: "pro plan price 20 dollars pro plan",
            head_summary: "pro plan repriced to 29",
            head_body: "billing updated",
        },
        Case {
            query: "number of retries allowed",
            stale_summary: "retries allowed 3",
            stale_body: "number of retries allowed is 3 retries allowed",
            head_summary: "allowed retries now 5",
            head_body: "config changed",
        },
        Case {
            query: "current team lead for checkout",
            stale_summary: "checkout team lead is Priya",
            stale_body: "checkout team lead Priya checkout team lead",
            head_summary: "checkout lead now Marcus",
            head_body: "handover done",
        },
        Case {
            query: "where do I live now city",
            stale_summary: "I live in Austin city",
            stale_body: "I live in Austin city I live in Austin city",
            head_summary: "live in Denver city now",
            head_body: "relocation complete",
        },
    ]
}

fn build(store: &Store, c: &Case) -> (gungnir::EntryId, gungnir::EntryId) {
    let mut stale = Entry::new("a", EntryKind::Decision, c.stale_summary);
    stale.body = c.stale_body.into();
    stale.timestamp = Utc::now() - Duration::days(30);
    stale.verify("archive", None);
    store.create(&stale).unwrap();

    let mut head = Entry::new("a", EntryKind::Decision, c.head_summary);
    head.body = c.head_body.into();
    head.timestamp = Utc::now() - Duration::days(1);
    head.revises = Some(stale.id);
    head.verify("archive", None);
    store.create(&head).unwrap();

    (stale.id, head.id)
}

fn main() {
    let n = cases().len();
    let mut stale_first = 0usize;
    let mut head_absent = 0usize;
    let mut per_case = Vec::new();

    for c in cases() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (stale_id, head_id) = build(&store, &c);

        // Plain present-time query: no --current, the AEV-25 ranking
        // condition that produced 24/40 stale>latest.
        let out = gungnir::recall::search(&store, &Query::new(c.query, 10)).unwrap();
        let pos = |id| out.iter().position(|h| h.entry.id == id);
        let stale = pos(stale_id);
        let head = pos(head_id);
        let stale_wins = match (stale, head) {
            (Some(s), Some(h)) => s < h,
            (Some(_), None) => true,
            _ => false,
        };
        if stale_wins {
            stale_first += 1;
        }
        if head.is_none() {
            head_absent += 1;
        }
        per_case.push(format!(
            "  {:<42} stale={:?} head={:?} stale_first={}",
            format!("{:?}", c.query),
            stale.map(|i| i + 1),
            head.map(|i| i + 1),
            stale_wins,
        ));
    }

    for line in &per_case {
        println!("{line}");
    }
    println!("stale_first={stale_first}/{n} head_absent={head_absent}/{n}");
}
