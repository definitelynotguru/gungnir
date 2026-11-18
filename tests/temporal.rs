//! Temporal recall: chain-head resolution (`current_only`), point-in-time
//! evaluation (`as_of`), and the coverage counts behind abstention.
//!
//! Timestamps are set by hand on entries and log records, so tests are
//! deterministic without sleeping.

use chrono::{DateTime, TimeZone, Utc};

use gungnir::{Entry, EntryKind, Gungnir, Query, VerificationRecord};

fn at(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0).unwrap()
}

/// 2026-01-01T00:00:00Z plus `n` hours.
fn h(n: i64) -> DateTime<Utc> {
    at(1_767_225_600 + n * 3600)
}

fn date_at(year: i32, month: u32, day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 12, 0, 0).unwrap()
}

fn dated_entry(summary: &str, body: &str, timestamp: DateTime<Utc>) -> Entry {
    let mut entry = Entry::new("eval", EntryKind::Session, summary);
    entry.timestamp = timestamp;
    entry.body = body.into();
    entry
}

fn codex_recall(g: &Gungnir, query: Query) -> gungnir::SearchOutcome {
    g.search_layer(gungnir::Layer::Codex, &query).unwrap()
}

#[test]
fn current_only_resolves_chains_to_heads() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
    codex.create(&v1).unwrap();
    let mut v2 = Entry::new("a", EntryKind::Decision, "sessions use postgres");
    v2.revises = Some(v1.id);
    v2.verify("team", None);
    codex.create(&v2).unwrap();

    let out = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("sessions use", 10).current(),
        )
        .unwrap();
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].entry.summary, "sessions use postgres");
    assert_eq!(out.coverage.hidden_superseded, 1);

    // Plain recall still surfaces both generations.
    let all = g
        .search_layer(gungnir::Layer::Codex, &Query::new("sessions use", 10))
        .unwrap();
    assert_eq!(all.hits.len(), 2);
}

#[test]
fn current_only_drops_contradicted_facts() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let target = Entry::new("a", EntryKind::Observation, "counter evidence lives here");
    codex.create(&target).unwrap();
    let mut claim = Entry::new("a", EntryKind::Observation, "load balancer flapping");
    claim.contradict(target.id, "ops", None);
    codex.create(&claim).unwrap();

    let out = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("load balancer", 10).current(),
        )
        .unwrap();
    assert!(out.hits.is_empty());
    assert_eq!(out.coverage.contradicted, 1);

    // Non-current recall keeps it visible but ranked below unverified facts.
    let plain = g
        .search_layer(gungnir::Layer::Codex, &Query::new("load balancer", 10))
        .unwrap();
    assert_eq!(plain.hits.len(), 1);
}

#[test]
fn as_of_evaluates_verification_from_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let mut e = Entry::new("a", EntryKind::Decision, "queue backed by redis");
    e.timestamp = h(1);
    codex.create(&e).unwrap();

    // Verified at hour 5, rolled back at hour 20, recorded by hand so the
    // wall clock never matters.
    let mut verified = e.clone();
    verified.verify("human", None);
    verified.verification_log[0].timestamp = h(5);
    codex
        .update_with(&verified, &|id| codex.exists(id))
        .unwrap();

    let mut rolled = verified;
    rolled.mark_rolled_back("agent");
    rolled.verification_log.last_mut().unwrap().timestamp = h(20);
    codex.update_with(&rolled, &|id| codex.exists(id)).unwrap();

    let q = |at: DateTime<Utc>| {
        g.search_layer(
            gungnir::Layer::Codex,
            &Query::new("queue redis", 10).as_of(at),
        )
        .unwrap()
    };

    // Before verification: unverified bucket, still visible.
    let early = q(h(3));
    assert_eq!(early.coverage.unverified, 1);

    // Between verify and rollback: counted as verified.
    let mid = q(h(10));
    assert_eq!(mid.coverage.verified, 1);
    assert_eq!(mid.hits.len(), 1);

    // After rollback: hidden entirely, with the count proving why.
    let late = q(h(30));
    assert!(late.hits.is_empty());
    assert_eq!(late.coverage.hidden_rolled_back, 1);

    // Present time (no as_of): same as after rollback.
    assert!(g
        .search_layer(gungnir::Layer::Codex, &Query::new("queue redis", 10))
        .unwrap()
        .hits
        .is_empty());
}

#[test]
fn as_of_hides_revisions_that_dont_exist_yet() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let mut v1 = Entry::new("a", EntryKind::Decision, "deploys happen fridays");
    v1.timestamp = h(1);
    codex.create(&v1).unwrap();
    let mut v2 = Entry::new("a", EntryKind::Decision, "deploys happen mondays");
    v2.timestamp = h(50);
    v2.revises = Some(v1.id);
    codex.create(&v2).unwrap();

    // Point in time before v2 exists: only v1 is even a candidate.
    let early = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("deploys happen", 10).as_of(h(10)).current(),
        )
        .unwrap();
    assert_eq!(early.hits.len(), 1);
    assert_eq!(early.hits[0].entry.summary, "deploys happen fridays");

    // After v2 lands: current view resolves to it.
    let late = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("deploys happen", 10).as_of(h(60)).current(),
        )
        .unwrap();
    assert_eq!(late.hits.len(), 1);
    assert_eq!(late.hits[0].entry.summary, "deploys happen mondays");
    assert_eq!(late.coverage.hidden_superseded, 1);
}

#[test]
fn brief_reports_coverage_and_abstains_without_verified_knowledge() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    // Unverified Codex fact only: nothing verified anywhere.
    let mut d = Entry::new(
        "builder",
        EntryKind::Decision,
        "cache hit rate tuning guide",
    );
    d.body = "cache tuning notes".into();
    codex.create(&d).unwrap();

    let b = g.brief("builder", "cache tuning", 8).unwrap();
    assert!(
        b.markdown
            .contains("Entries matched this task but none are verified"),
        "{}",
        b.markdown
    );
    assert!(b.markdown.contains("## Coverage"));

    g.verify(d.id, "human", None).unwrap();
    let b2 = g.brief("builder", "cache tuning", 8).unwrap();
    assert!(
        !b2.markdown.contains("No verified knowledge"),
        "{}",
        b2.markdown
    );
}

#[test]
fn log_records_roundtrip_with_custom_timestamps() {
    let rec = VerificationRecord {
        verifier: "ci".into(),
        timestamp: h(2),
        status: gungnir::RecordStatus::Verified,
        note: None,
    };
    let yaml = serde_yaml::to_string(&rec).unwrap();
    let back: VerificationRecord = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(back, rec);
}

#[test]
fn as_of_includes_entry_and_log_at_exact_cutoff() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let mut e = Entry::new("a", EntryKind::Decision, "queue backed by redis");
    e.timestamp = h(10);
    codex.create(&e).unwrap();
    let mut verified = e.clone();
    verified.verify("human", None);
    verified.verification_log[0].timestamp = h(10);
    codex
        .update_with(&verified, &|id| codex.exists(id))
        .unwrap();

    let at_cutoff = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("queue redis", 10).as_of(h(10)),
        )
        .unwrap();
    assert_eq!(at_cutoff.coverage.verified, 1);
    assert_eq!(at_cutoff.hits.len(), 1);

    let before = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("queue redis", 10).as_of(h(9)),
        )
        .unwrap();
    assert!(before.hits.is_empty());
    assert_eq!(before.coverage, gungnir::Coverage::default());
}

#[test]
fn as_of_plus_current_keeps_tail_when_later_revisions_are_in_the_future() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let mut v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
    v1.timestamp = h(10);
    codex.create(&v1).unwrap();
    let mut v2 = Entry::new("a", EntryKind::Observation, "interim migration note");
    v2.timestamp = h(50);
    v2.revises = Some(v1.id);
    codex.create(&v2).unwrap();
    let mut v3 = Entry::new("a", EntryKind::Decision, "sessions use postgres");
    v3.timestamp = h(60);
    v3.revises = Some(v2.id);
    codex.create(&v3).unwrap();

    let mid = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("sessions use", 10).as_of(h(30)).current(),
        )
        .unwrap();
    assert_eq!(mid.hits.len(), 1);
    assert_eq!(mid.hits[0].entry.summary, "sessions use mysql");
}

#[test]
fn as_of_evaluates_learned_time_not_event_time() {
    // Semantics documented in SPEC.md: `as_of` asks "what did the store
    // believe at instant T" — the entry's `timestamp` is when the fact was
    // *learned/recorded*, not when the event happened. An as_of between a
    // fact-change in the world and the moment the new fact was learned
    // still returns the old head.
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    // The world changed at h(20); the store learned about it at h(50).
    let mut v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
    v1.timestamp = h(10);
    codex.create(&v1).unwrap();

    let mut v2 = Entry::new("a", EntryKind::Decision, "sessions use postgres");
    v2.timestamp = h(50);
    v2.revises = Some(v1.id);
    codex.create(&v2).unwrap();

    // As-of between the event and its recording: the old head still stands.
    let mid = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("sessions use", 10).as_of(h(30)).current(),
        )
        .unwrap();
    assert_eq!(mid.hits.len(), 1);
    assert_eq!(mid.hits[0].entry.summary, "sessions use mysql");

    // After the learning instant: the new head.
    let late = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("sessions use", 10).as_of(h(60)).current(),
        )
        .unwrap();
    assert_eq!(late.hits[0].entry.summary, "sessions use postgres");
}

#[test]
fn as_of_replays_a_supersede_and_rollback_sequence() {
    // Replay the same chain at three instants: before the revision,
    // between revision and rollback, after rollback. The ancestor set
    // visible at each instant must be exactly right.
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let mut v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
    v1.timestamp = h(10);
    v1.verify("human", None);
    v1.verification_log[0].timestamp = h(12);
    codex.create(&v1).unwrap();

    let v2_id = g
        .supersede(v1.id, "a", "sessions use postgres", "migration")
        .unwrap()
        .id;
    let mut v2 = codex.require(v2_id).unwrap();
    v2.timestamp = h(50);
    codex.update_with(&v2, &|id| codex.exists(id)).unwrap();

    let rb_id = g.rollback(v2_id, "a").unwrap();
    let mut rb = codex.require(rb_id).unwrap();
    rb.timestamp = h(62);
    codex.update_with(&rb, &|id| codex.exists(id)).unwrap();
    let mut v2 = codex.require(v2_id).unwrap();
    v2.verification_log[0].timestamp = h(60); // the rolled_back record
    codex.update_with(&v2, &|id| codex.exists(id)).unwrap();

    let q = |at: DateTime<Utc>| {
        g.search_layer(
            gungnir::Layer::Codex,
            &Query::new("sessions use", 10).as_of(at).current(),
        )
        .unwrap()
    };

    // Before the revision: v1 is the only head.
    let early = q(h(30));
    assert_eq!(early.hits.len(), 1);
    assert_eq!(early.hits[0].entry.summary, "sessions use mysql");
    assert_eq!(early.coverage.hidden_superseded, 0);

    // Between revision and rollback: v2 is the head, v1 hidden.
    let mid = q(h(55));
    assert_eq!(mid.hits.len(), 1);
    assert_eq!(mid.hits[0].entry.summary, "sessions use postgres");
    assert_eq!(mid.coverage.hidden_superseded, 1);

    // After the rollback: v2 is rolled back and hidden, v1 is restored.
    let late = q(h(65));
    assert_eq!(late.hits.len(), 1);
    assert_eq!(late.hits[0].entry.summary, "sessions use mysql");
    assert_eq!(late.coverage.hidden_rolled_back, 1);
    assert_eq!(late.coverage.hidden_superseded, 0);
}

#[test]
fn rollback_restores_ancestor_without_superseding_it() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    // v1 verified, superseded by v2, then v2 rolled back. The rollback
    // record's `revises` points at v1 to restore it — it must not count
    // as another revision superseding v1.
    let mut v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
    v1.verify("human", None);
    codex.create(&v1).unwrap();
    let v2 = g
        .supersede(v1.id, "a", "sessions use postgres", "migration done")
        .unwrap()
        .id;
    g.rollback(v2, "a").unwrap();

    // Current view falls back to v1, the last live revision, and hides
    // the rolled-back v2.
    let out = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("sessions use", 10).current(),
        )
        .unwrap();
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].entry.summary, "sessions use mysql");
    assert_eq!(out.coverage.hidden_superseded, 0);
    assert_eq!(out.coverage.hidden_rolled_back, 1);

    // Nothing is superseded: not in the id set, not in brief labels,
    // not in stats.
    assert!(!gungnir::gungnir::superseded_ids(&codex)
        .unwrap()
        .contains(&v1.id));

    let b = g.brief("a", "sessions use", 8).unwrap();
    assert!(
        b.markdown.contains("- [verified] sessions use mysql"),
        "{}",
        b.markdown
    );

    let stats = g.stats(None).unwrap();
    assert_eq!(stats.superseded, 0);
}

#[test]
fn require_verified_as_of_reports_the_state_it_filtered_on() {
    // A verified-only temporal query must serialize the *historical* state
    // it filtered by, not today's frontmatter state.
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let counter = Entry::new("a", EntryKind::Observation, "counter data");
    codex.create(&counter).unwrap();

    let mut e = Entry::new("a", EntryKind::Decision, "sessions pool at ten");
    e.timestamp = h(0);
    e.verify("team", None);
    e.verification_log[0].timestamp = h(0);
    e.verification_log.push(VerificationRecord {
        verifier: "ops".into(),
        timestamp: h(2),
        status: gungnir::RecordStatus::Contradicted,
        note: None,
    });
    e.verification = gungnir::VerificationState::Contradicted { by: counter.id };
    codex.create(&e).unwrap();

    let out = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("sessions pool", 10)
                .require_verified()
                .as_of(h(1)),
        )
        .unwrap();
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].state, gungnir::StateAt::Verified);
    // The entry itself is contradicted *now*; only the hit's state is temporal.
    assert!(matches!(
        out.hits[0].entry.verification,
        gungnir::VerificationState::Contradicted { .. }
    ));
}

#[test]
fn require_verified_current_abstains_on_superseded_verified_tail() {
    // require_verified and current must filter the same pool: a verified
    // tail hidden behind an unverified head yields no hits and an honest
    // zero-coverage abstention, never a silent empty answer.
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let mut v1 = Entry::new("a", EntryKind::Decision, "sessions use postgres pooling");
    v1.timestamp = h(1);
    codex.create(&v1).unwrap();
    g.verify(v1.id, "team", None).unwrap();
    g.supersede(v1.id, "a", "sessions use redis pooling", "")
        .unwrap();

    let out = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("sessions use", 10).current().require_verified(),
        )
        .unwrap();
    assert!(out.hits.is_empty(), "{:?}", out.hits);
    assert_eq!(out.coverage.verified, 0);
    assert_eq!(out.coverage.hidden_superseded, 1);
}

#[test]
fn absolute_date_retrieves_relative_entry_without_topic_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let entry = dated_entry(
        "smoker purchase",
        "I bought a smoker ten days ago.",
        date_at(2026, 11, 18),
    );
    g.codex().unwrap().create(&entry).unwrap();

    let out = codex_recall(
        &g,
        Query::new("Which object is dated 2026-11-08?", 10).require_evidence(),
    );
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].entry.id, entry.id);
    assert!(out.hits[0].score > 0.0);
    assert_eq!(out.coverage.unverified, 1);
}

#[test]
fn relative_query_resolves_against_explicit_question_time() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let entry = dated_entry(
        "smoker purchase",
        "Bought a smoker on 2026-11-08.",
        date_at(2026, 11, 18),
    );
    g.codex().unwrap().create(&entry).unwrap();

    let out = codex_recall(
        &g,
        Query::new("Which kitchen appliance did I buy 10 days ago?", 10)
            .question_time(date_at(2026, 11, 18)),
    );
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].entry.id, entry.id);
}

#[test]
fn omitted_question_time_does_not_resolve_relative_dates_from_wall_clock() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let entry = dated_entry(
        "smoker purchase",
        "Bought a smoker on 2026-11-08.",
        date_at(2026, 11, 18),
    );
    g.codex().unwrap().create(&entry).unwrap();
    let text = "Which kitchen appliance did I buy 10 days ago?";

    let unresolved = codex_recall(&g, Query::new(text, 10));
    assert!(unresolved.hits.is_empty());
    assert_eq!(unresolved.coverage.total_matched(), 0);

    let resolved = codex_recall(
        &g,
        Query::new(text, 10).question_time(date_at(2026, 11, 18)),
    );
    assert_eq!(resolved.hits.len(), 1);
}

#[test]
fn entry_relative_dates_use_each_entry_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let november = dated_entry(
        "november purchase",
        "I bought a smoker ten days ago.",
        date_at(2026, 11, 18),
    );
    let october = dated_entry(
        "october purchase",
        "I bought a smoker ten days ago.",
        date_at(2026, 10, 18),
    );
    let codex = g.codex().unwrap();
    codex.create(&november).unwrap();
    codex.create(&october).unwrap();

    let out = codex_recall(&g, Query::new("Which object is dated 2026-11-08?", 10));
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].entry.id, november.id);
}

#[test]
fn wrong_date_has_no_hits_without_another_retrieval_signal() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let entry = dated_entry(
        "smoker purchase",
        "Bought a smoker on 2026-11-08.",
        date_at(2026, 11, 18),
    );
    g.codex().unwrap().create(&entry).unwrap();

    let out = codex_recall(
        &g,
        Query::new("Which kitchen appliance did I buy a week ago?", 10)
            .question_time(date_at(2026, 11, 18)),
    );
    assert!(out.hits.is_empty());
    assert_eq!(out.coverage.total_matched(), 0);
}

#[test]
fn parsed_date_without_index_hits_preserves_lexical_fallback_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let entry = dated_entry(
        "smoker purchase",
        "Bought a smoker on 2026-11-08.",
        date_at(2026, 11, 18),
    );
    g.codex().unwrap().create(&entry).unwrap();
    let text = "smoker purchase a week ago";

    let ordinary = codex_recall(&g, Query::new(text, 10));
    let fallback = codex_recall(
        &g,
        Query::new(text, 10).question_time(date_at(2026, 11, 18)),
    );
    assert_eq!(fallback.coverage, ordinary.coverage);
    assert_eq!(fallback.hits.len(), ordinary.hits.len());
    for (actual, expected) in fallback.hits.iter().zip(&ordinary.hits) {
        assert_eq!(actual.entry.id, expected.entry.id);
        assert_eq!(actual.score.to_bits(), expected.score.to_bits());
    }
    assert_eq!(fallback.hits[0].entry.id, entry.id);
}

#[test]
fn multiple_query_dates_union_without_duplicating_entries() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let first = dated_entry("first object", "Recorded on 2026-11-08.", h(1));
    let second = dated_entry("second object", "Recorded on 2026-11-11.", h(2));
    let lexical_only = dated_entry("events notes", "No date in this entry.", h(3));
    let codex = g.codex().unwrap();
    for entry in [&first, &second, &lexical_only] {
        codex.create(entry).unwrap();
    }

    let out = codex_recall(
        &g,
        Query::new("events on 2026-11-08 or 2026-11-11 and a week ago", 10)
            .question_time(date_at(2026, 11, 18)),
    );
    assert_eq!(out.hits.len(), 2);
    assert!(out.hits.iter().any(|hit| hit.entry.id == first.id));
    assert!(out.hits.iter().any(|hit| hit.entry.id == second.id));
    assert_eq!(out.coverage.total_matched(), 2);
}

#[test]
fn date_queries_keep_as_of_admission_and_verification_history_independent() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();
    let mut counter = Entry::new("a", EntryKind::Observation, "counter evidence");
    counter.timestamp = h(0);
    codex.create(&counter).unwrap();
    let mut entry = dated_entry("smoker purchase", "Bought a smoker on 2025-12-22.", h(10));
    entry.verify("team", None);
    entry.verification_log[0].timestamp = h(12);
    entry.contradict(counter.id, "ops", None);
    entry.verification_log[1].timestamp = h(20);
    codex.create(&entry).unwrap();
    let query = |cutoff| {
        Query::new("Which appliance did I buy 10 days ago?", 10)
            .question_time(h(10))
            .as_of(cutoff)
            .require_verified()
    };

    let before_entry = codex_recall(&g, query(h(9)));
    assert!(before_entry.hits.is_empty());
    assert_eq!(before_entry.coverage.total, 1);

    let verified = codex_recall(&g, query(h(15)));
    assert_eq!(verified.hits.len(), 1);
    assert_eq!(verified.hits[0].state, gungnir::StateAt::Verified);

    let contradicted = codex_recall(&g, query(h(25)));
    assert!(contradicted.hits.is_empty());
    assert_eq!(contradicted.coverage.verified, 0);
    assert_eq!(contradicted.coverage.contradicted, 1);
}

#[test]
fn date_hits_preserve_current_only_and_latest_slot_order() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();
    let mut tail = dated_entry(
        "smoker smoker smoker purchase",
        "Bought on 2025-12-22.",
        h(10),
    );
    tail.verify("team", None);
    codex.create(&tail).unwrap();
    let mut head = dated_entry("smoker purchase", "Bought on 2025-12-22.", h(11));
    head.revises = Some(tail.id);
    head.verify("team", None);
    codex.create(&head).unwrap();
    let query = || Query::new("smoker 10 days ago", 10).question_time(h(10));

    let plain = codex_recall(&g, query());
    assert_eq!(plain.hits.len(), 2);
    assert_eq!(plain.hits[0].entry.id, head.id);

    let current = codex_recall(&g, query().current());
    assert_eq!(current.hits.len(), 1);
    assert_eq!(current.hits[0].entry.id, head.id);
    assert_eq!(current.coverage.hidden_superseded, 1);
}

#[test]
fn strict_provenance_filters_date_hits_without_changing_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();
    let unverified = dated_entry("unverified object", "Dated 2026-11-08.", h(1));
    let mut verified = dated_entry("verified object", "Dated 2026-11-08.", h(2));
    verified.verify("team", None);
    codex.create(&unverified).unwrap();
    codex.create(&verified).unwrap();

    let out = codex_recall(
        &g,
        Query::new("Which object is dated 2026-11-08?", 10).require_verified(),
    );
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.hits[0].entry.id, verified.id);
    assert_eq!(out.coverage.verified, 1);
    assert_eq!(out.coverage.unverified, 1);
}

#[test]
fn date_masking_preserves_role_number_and_negation_answer_constraints() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();
    let correct = dated_entry(
        "release note",
        "The senior engineer did not ship version 42 on 2026-11-08.",
        h(1),
    );
    let wrong_role = dated_entry(
        "junior note",
        "The junior engineer did not ship version 42 on 2026-11-08.",
        h(2),
    );
    let wrong_number = dated_entry(
        "other version",
        "The senior engineer did not ship version 43 on 2026-11-08.",
        h(3),
    );
    let missing_negation = dated_entry(
        "positive note",
        "The senior engineer shipped version 42 on 2026-11-08.",
        h(4),
    );
    for entry in [&correct, &wrong_role, &wrong_number, &missing_negation] {
        codex.create(entry).unwrap();
    }

    let out = codex_recall(
        &g,
        Query::new(
            "Did the senior engineer not ship version 42 on 2026-11-08?",
            10,
        )
        .require_evidence(),
    );
    assert_eq!(out.hits.len(), 1, "{:?}", out.hits);
    assert_eq!(out.hits[0].entry.id, correct.id);
    assert_eq!(out.coverage.total_matched(), 4);
}

#[test]
fn only_verified_date_hits_seed_verified_neighbor_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();
    let mut verified_seed = dated_entry("verified seed", "Dated 2026-11-08.", h(1));
    verified_seed.entities = vec!["shared".into()];
    verified_seed.verify("team", None);
    let mut unverified_seed = dated_entry("unverified seed", "Dated 2026-11-08.", h(2));
    unverified_seed.entities = vec!["unverified".into()];
    let mut verified_neighbor = dated_entry("verified neighbor", "No date here.", h(3));
    verified_neighbor.entities = vec!["shared".into()];
    verified_neighbor.verify("team", None);
    let mut second_verified_neighbor = dated_entry("second neighbor", "No date here.", h(4));
    second_verified_neighbor.entities = vec!["shared".into()];
    second_verified_neighbor.verify("team", None);
    let mut blocked_neighbor = dated_entry("blocked neighbor", "No date here.", h(5));
    blocked_neighbor.entities = vec!["unverified".into()];
    blocked_neighbor.verify("team", None);
    for entry in [
        &verified_seed,
        &unverified_seed,
        &verified_neighbor,
        &second_verified_neighbor,
        &blocked_neighbor,
    ] {
        codex.create(entry).unwrap();
    }

    let out = codex_recall(&g, Query::new("Which object is dated 2026-11-08?", 10));
    let ids = out.hits.iter().map(|hit| hit.entry.id).collect::<Vec<_>>();
    assert!(ids.contains(&verified_seed.id));
    assert!(ids.contains(&unverified_seed.id));
    assert!(ids.contains(&verified_neighbor.id));
    assert!(ids.contains(&second_verified_neighbor.id));
    assert!(!ids.contains(&blocked_neighbor.id));
    assert_eq!(out.coverage.total_matched(), 2);
}

#[test]
fn invalid_date_forms_leave_ordinary_lexical_results_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let entry = dated_entry("smoker purchase", "No supported date here.", h(1));
    g.codex().unwrap().create(&entry).unwrap();
    let ordinary = codex_recall(&g, Query::new("smoker purchase", 10));

    for form in [
        "1 days ago",
        "2 day ago",
        "0 days ago",
        "10 months ago",
        "2026-02-30",
        "2026-01-01T12:00:00Z",
    ] {
        let out = codex_recall(
            &g,
            Query::new(format!("smoker purchase {form}"), 10).question_time(date_at(2026, 11, 18)),
        );
        assert_eq!(out.hits.len(), 1, "{form}");
        assert_eq!(out.hits[0].entry.id, entry.id, "{form}");
        assert_eq!(
            out.hits[0].score.to_bits(),
            ordinary.hits[0].score.to_bits(),
            "{form}"
        );
    }
}

#[test]
fn date_miss_preserves_numeric_answer_constraints_on_lexical_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let entry = dated_entry(
        "smoker purchase",
        "Bought a smoker on 2026-11-09.",
        date_at(2026, 11, 18),
    );
    g.codex().unwrap().create(&entry).unwrap();

    let query = Query::new("smoker purchase 2026-11-08", 10).require_evidence();
    let out = codex_recall(&g, query.clone());
    assert!(out.hits.is_empty());
    assert_eq!(out.coverage.unverified, 1);
    assert_eq!(out.evidence_filtered, 1);
    assert!(out.abstained(&query));
}

#[test]
fn date_hits_keep_existing_keyword_relevance_as_a_tie_break() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();
    let weaker = dated_entry("smoker purchase", "Dated 2026-11-08.", h(1));
    let stronger = dated_entry("smoker smoker purchase", "Dated 2026-11-08.", h(1));
    codex.create(&weaker).unwrap();
    codex.create(&stronger).unwrap();

    let out = codex_recall(&g, Query::new("smoker 2026-11-08", 10));
    assert_eq!(out.hits.len(), 2);
    assert_eq!(out.hits[0].entry.id, stronger.id);
    assert!(out.hits[0].score > out.hits[1].score);
}
