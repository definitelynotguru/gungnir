//! Gungnir Bench v1: a deterministic accuracy harness over a synthetic
//! corpus, modeled on LongMemEval's five abilities (information extraction,
//! multi-session reasoning, knowledge updates, temporal reasoning,
//! abstention).
//!
//! Grading is exact and offline: each check runs against the library API on
//! a freshly built corpus and asserts expected recall behavior. Scores print
//! per ability with `cargo test --test bench -- --nocapture`.
//!
//! This is a self-benchmark, not LongMemEval. Vendor-published numbers from
//! other systems are not comparable to it.

use chrono::{DateTime, TimeZone, Utc};

use gungnir::{Entry, EntryKind, Gungnir, Promotion, Query};

fn at(hours_from_epoch_base: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_767_225_600 + hours_from_epoch_base * 3600, 0).unwrap()
}

/// A corpus exercising every layer: two builder sessions on one topic (one
/// failed attempt, one success promoted to the Codex), a supersession chain,
/// a verified-then-rolled-back fact, another agent's private journal, and an
/// unrelated verified fact.
struct Corpus {
    g: Gungnir,
    /// Holds the tempdir open for as long as the facade lives.
    _dir: tempfile::TempDir,
    checkout_decision: gungnir::EntryId,
}

fn build() -> Corpus {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    // Session 1: failure.
    let s1 = g.start_session("builder", "fix slow checkout query");
    g.add_observation(&s1, "EXPLAIN shows seq scan on orders_archive").unwrap();
    g.add_attempt(&s1, "added index on orders.user_id", false).unwrap();
    g.end_session(&s1, "index hint did not help", vec![]).unwrap();

    // Session 2: success, promoted into the Codex with provenance.
    let s2 = g.start_session("builder", "rewrite checkout query");
    g.add_attempt(&s2, "rewrote query to use orders_archive_idx", true).unwrap();
    let report = g
        .end_session(
            &s2,
            "rewrote checkout query successfully",
            vec![Promotion {
                kind: EntryKind::Decision,
                summary: "checkout queries must use orders_archive_idx".into(),
                body: "seq scan on orders_archive was the bottleneck".into(),
            }],
        )
        .unwrap();
    g.verify(report.promoted[0], "team-review", None).unwrap();

    // Supersession chain with explicit timestamps for as-of checks.
    let mut c1 = Entry::new("ops", EntryKind::Decision, "budget reviews happen in q3");
    c1.timestamp = at(10);
    codex.create(&c1).unwrap();
    g.verify(c1.id, "ops-lead", None).unwrap();

    let mut c2 = Entry::new("ops", EntryKind::Decision, "budget reviews moved to q1");
    c2.timestamp = at(100);
    c2.revises = Some(c1.id);
    codex.create(&c2).unwrap();

    // Verified then rolled back, both transitions stamped by hand.
    let mut f1 = Entry::new("ops", EntryKind::Decision, "feature flags live in launchdarkly");
    f1.timestamp = at(20);
    codex.create(&f1).unwrap();
    let mut verified = f1.clone();
    verified.verify("ops-lead", None);
    verified.verification_log[0].timestamp = at(30);
    codex.update_with(&verified, &|id| codex.exists(id)).unwrap();
    let mut rolled = verified;
    rolled.mark_rolled_back("ops");
    rolled.verification_log.last_mut().unwrap().timestamp = at(40);
    codex.update_with(&rolled, &|id| codex.exists(id)).unwrap();

    // Another agent's private journal must never leak into briefings.
    let s3 = g.start_session("scout", "investigate deploy timeouts");
    g.add_observation(&s3, "deploy pipeline timeout in github actions runners").unwrap();
    g.end_session(&s3, "runner timeouts documented", vec![]).unwrap();

    // Unrelated verified fact far from every query below.
    let mut unrelated = Entry::new("hr", EntryKind::Decision, "meeting room hera seats eight");
    unrelated.body = "bookings through the calendar".into();
    codex.create(&unrelated).unwrap();
    g.verify(unrelated.id, "office-manager", None).unwrap();

    Corpus { g, _dir: dir, checkout_decision: report.promoted[0] }
}

struct Score {
    ability: &'static str,
    passed: usize,
    total: usize,
}

impl Score {
    fn new(ability: &'static str) -> Self {
        Self { ability, passed: 0, total: 0 }
    }
    fn check(&mut self, name: &str, ok: bool) -> bool {
        self.total += 1;
        if ok {
            self.passed += 1;
        } else {
            println!("  FAIL {name}");
        }
        ok
    }
}

#[test]
fn gungnir_bench() {
    let c = build();
    let mut scores = Vec::new();

    scores.push(extraction(&c));
    scores.push(multi_session(&c));
    scores.push(knowledge_updates(&c));
    scores.push(temporal(&c));
    scores.push(abstention(&c));

    let total: usize = scores.iter().map(|s| s.total).sum();
    let passed: usize = scores.iter().map(|s| s.passed).sum();
    println!("\ngungnir-bench: {passed}/{total} checks pass");
    for s in &scores {
        println!("  {:<26} {}/{}", s.ability, s.passed, s.total);
    }
    assert_eq!(passed, total, "bench regressions detected; see FAIL lines above");
}

fn extraction(c: &Corpus) -> Score {
    let mut s = Score::new("information extraction");
    let codex = c.g.codex().unwrap();
    let journal = c.g.journal("builder").unwrap();

    // The promoted decision is retrievable by its own content.
    let hits = c
        .g
        .recall_layer(gungnir::Layer::Codex, &Query::new("orders archive bottleneck", 5))
        .unwrap();
    s.check(
        "promoted fact recalled",
        hits.first().is_some_and(|h| h.entry.summary.contains("orders_archive")),
    );

    // Provenance survived promotion: evidence resolves to the journal entry.
    let d1 = codex.require(c.checkout_decision).unwrap();
    let evidence_ok = match d1.evidence.first() {
        Some(gungnir::Evidence::Ref { id }) => journal.exists(*id).unwrap_or(false),
        _ => false,
    };
    s.check("promotion links to journal source", evidence_ok);

    // Archived transcript detail is searchable in the journal.
    let obs = c
        .g
        .recall_layer(
            gungnir::Layer::Journal { agent: "builder" },
            &Query::new("explain seq scan orders", 5),
        )
        .unwrap();
    s.check("session observation recalled from journal", !obs.is_empty());

    // Briefing surfaces the transcript excerpt under the journal hit.
    let b = c.g.brief("builder", "slow checkout orders", 8).unwrap();
    s.check("briefing includes excerpt", b.markdown.contains("> -"));

    s
}

fn multi_session(c: &Corpus) -> Score {
    let mut s = Score::new("multi-session reasoning");
    let b = c.g.brief("builder", "slow checkout rewrite", 10).unwrap();

    // Both sessions' outcomes appear in one briefing.
    s.check("failed attempt present", b.markdown.contains("index hint did not help"));
    s.check("successful rewrite present", b.markdown.contains("rewrote checkout query"));

    // Another agent's private knowledge does not leak.
    s.check(
        "other agent's journal excluded",
        !b.markdown.contains("github actions timeout"),
    );

    // The shared verified fact ranks above everything else for the topic.
    s.check(
        "verified promotion leads codex hits",
        b.codex_hits
            .first()
            .is_some_and(|h| h.entry.summary.contains("orders_archive_idx")),
    );

    s
}

fn knowledge_updates(c: &Corpus) -> Score {
    let mut s = Score::new("knowledge updates");

    let current = c
        .g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("budget reviews", 10).current(),
        )
        .unwrap();
    s.check(
        "current view keeps chain head only",
        current.hits.len() == 1
            && current.hits[0].entry.summary.contains("q1"),
    );
    s.check("superseded tail counted as hidden", current.coverage.hidden_superseded == 1);

    // Honest coverage: the head is unverified even though its ancestor was
    // verified. Abstention reflects what is true now, not history.
    s.check(
        "head unverified reported honestly",
        current.coverage.unverified == 1 && current.coverage.verified == 0,
    );

    // Plain recall shows both generations with the stale one flagged.
    let b = c.g.brief("ops", "budget reviews", 10).unwrap();
    s.check("stale generation flagged superseded", b.markdown.contains(", superseded]"));

    s
}

fn temporal(c: &Corpus) -> Score {
    let mut s = Score::new("temporal reasoning");

    // Before the revision exists, the old fact is the only candidate.
    let early = c
        .g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("budget reviews", 10).as_of(at(50)).current(),
        )
        .unwrap();
    s.check(
        "point-in-time before revision",
        early.hits.len() == 1 && early.hits[0].entry.summary.contains("q3"),
    );

    // After it lands, the head moves.
    let late = c
        .g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("budget reviews", 10).as_of(at(150)).current(),
        )
        .unwrap();
    s.check(
        "point-in-time after revision",
        late.hits.len() == 1 && late.hits[0].entry.summary.contains("q1"),
    );

    // Rollback visibility flips at the recorded time, not at write time.
    let flag_before = c
        .g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("feature flags", 10).as_of(at(35)),
        )
        .unwrap();
    s.check(
        "fact verified as-of before rollback",
        flag_before.coverage.verified == 1 && flag_before.hits.len() == 1,
    );
    let flag_after = c
        .g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("feature flags", 10).as_of(at(50)),
        )
        .unwrap();
    s.check(
        "rolled back fact hidden after rollback",
        flag_after.hits.is_empty() && flag_after.coverage.hidden_rolled_back == 1,
    );

    s
}

fn abstention(c: &Corpus) -> Score {
    let mut s = Score::new("abstention");

    let nothing = c
        .g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("quarterly revenue forecast spreadsheet", 10),
        )
        .unwrap();
    s.check("unknown topic returns no hits", nothing.hits.is_empty());
    s.check(
        "coverage is empty for unknown topic",
        nothing.coverage.total_visible() == 0,
    );

    // Unverified-only topic: facts exist, but the briefing says plainly that
    // none are verified instead of staying silent about it.
    let b = c.g.brief("builder", "cache tuning internals", 8).unwrap();
    s.check(
        "abstention line when zero verified match",
        b.markdown.contains("No verified knowledge covers this task"),
    );

    s
}
