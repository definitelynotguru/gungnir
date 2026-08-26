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
    claim.contradict(target.id, "ops");
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
            .contains("No verified knowledge covers this task"),
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
        status: "verified".into(),
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
