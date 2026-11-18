//! AEV-42 / GUNG-23 targeted recall expansion measurement.
//!
//! BM25 returns only the turns that share question words, so a verified
//! neighbor that shares an entity string or session id but no query tokens
//! cannot be recovered by keyword recall. This fixture builds eight fixed
//! cases over a fresh temp Store each: one verified lexical seed, one verified
//! neighbor linked by an `entity_mentions()` string or a `session_id`, and one
//! verified off-key distractor. The neighbor shares no stemmed token with the
//! query.
//!
//! Run on any revision with `cargo run --example gung23_measurement`. The
//! baseline for these neighbors is 0/N, because keyword recall cannot recover
//! them without expansion. The post-change expectation is N/N on the current
//! arm. This is a small deterministic fixture, not a full eval.
//!
//! Two arms run over the same Store. The `current` arm matches the GUNG-21
//! harness and measures recall with expansion. The `require_verified` arm is a
//! lexical-only arm with a different semantic role: strict provenance stays
//! lexical by design. To measure the true before baseline on the same query
//! shape, restore `src/recall.rs` from the parent revision and rerun this
//! example.

use gungnir::recall::Query;
use gungnir::{Entry, EntryKind, Store};

#[derive(Clone, Copy)]
enum Link {
    Entity,
    Session,
}

struct Case {
    query: &'static str,
    seed_summary: &'static str,
    link: Link,
    key: &'static str,
    neighbor_summary: &'static str,
    distractor_summary: &'static str,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            query: "zephyr release notes",
            seed_summary: "zephyr release notes filed",
            link: Link::Entity,
            key: "Harbor Trip",
            neighbor_summary: "quartz ledger",
            distractor_summary: "onyx register",
        },
        Case {
            query: "amber module rollout",
            seed_summary: "amber module rollout done",
            link: Link::Session,
            key: "session-amber",
            neighbor_summary: "velvet parchment",
            distractor_summary: "granite folio",
        },
        Case {
            query: "cobalt pipeline stage",
            seed_summary: "cobalt pipeline stage green",
            link: Link::Entity,
            key: "North Quay",
            neighbor_summary: "willow basket",
            distractor_summary: "marble tablet",
        },
        Case {
            query: "granite quota review",
            seed_summary: "granite quota review passed",
            link: Link::Session,
            key: "session-granite",
            neighbor_summary: "saffron ribbon",
            distractor_summary: "basalt scroll",
        },
        Case {
            query: "mercury ledger window",
            seed_summary: "mercury ledger window open",
            link: Link::Entity,
            key: "East Basin",
            neighbor_summary: "pewter spindle",
            distractor_summary: "copper dial",
        },
        Case {
            query: "obsidian roster draft",
            seed_summary: "obsidian roster draft ready",
            link: Link::Session,
            key: "session-obsidian",
            neighbor_summary: "linen satchel",
            distractor_summary: "slate booklet",
        },
        Case {
            query: "topaz budget memo",
            seed_summary: "topaz budget memo signed",
            link: Link::Entity,
            key: "South Pier",
            neighbor_summary: "cedar spindle",
            distractor_summary: "flint ledger",
        },
        Case {
            query: "indigo manifest batch",
            seed_summary: "indigo manifest batch sent",
            link: Link::Session,
            key: "session-indigo",
            neighbor_summary: "mulberry weave",
            distractor_summary: "chalk tablet",
        },
    ]
}

fn build(store: &Store, c: &Case) -> (gungnir::EntryId, gungnir::EntryId) {
    let mut seed = Entry::new("a", EntryKind::Decision, c.seed_summary);
    apply_link(&mut seed, c);
    seed.verify("archive", None);
    store.create(&seed).unwrap();

    let mut neighbor = Entry::new("a", EntryKind::Decision, c.neighbor_summary);
    apply_link(&mut neighbor, c);
    neighbor.verify("archive", None);
    store.create(&neighbor).unwrap();

    let mut distractor = Entry::new("a", EntryKind::Decision, c.distractor_summary);
    distractor.verify("archive", None);
    store.create(&distractor).unwrap();

    (seed.id, neighbor.id)
}

fn apply_link(entry: &mut Entry, c: &Case) {
    match c.link {
        Link::Entity => entry.entities = vec![c.key.into()],
        Link::Session => entry.session_id = Some(c.key.into()),
    }
}

fn main() {
    let n = cases().len();
    let mut recovered = 0usize;
    let mut lexical_first = 0usize;
    let mut coverage_unchanged = 0usize;
    let mut lexical_only_recovered = 0usize;
    let mut per_case = Vec::new();

    for c in cases() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let (seed_id, neighbor_id) = build(&store, &c);

        let current =
            gungnir::recall::search_with_coverage(&store, &Query::new(c.query, 10).current())
                .unwrap();
        let has_neighbor = current.hits.iter().any(|h| h.entry.id == neighbor_id);
        if has_neighbor {
            recovered += 1;
        }
        if current.hits.first().is_some_and(|h| h.entry.id == seed_id) {
            lexical_first += 1;
        }
        if current.coverage.total_matched() == 1 {
            coverage_unchanged += 1;
        }

        let lexical_only = gungnir::recall::search_with_coverage(
            &store,
            &Query::new(c.query, 10).require_verified(),
        )
        .unwrap();
        let baseline = lexical_only.hits.iter().any(|h| h.entry.id == neighbor_id);
        if baseline {
            lexical_only_recovered += 1;
        }

        per_case.push(format!(
            "  {:<26} link={:<7} current_recovered={} lexical_only_recovered={}",
            format!("{:?}", c.query),
            match c.link {
                Link::Entity => "entity",
                Link::Session => "session",
            },
            has_neighbor,
            baseline,
        ));
    }

    for line in &per_case {
        println!("{line}");
    }
    println!("current arm (GUNG-21 shape, with expansion):");
    println!(
        "  recovered={recovered}/{n} lexical_first={lexical_first}/{n} coverage_unchanged={coverage_unchanged}/{n}"
    );
    println!("lexical-only arm (require_verified, strict provenance stays lexical):");
    println!("  recovered={lexical_only_recovered}/{n}");
}
