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
    Utc.timestamp_opt(1_767_225_600 + hours_from_epoch_base * 3600, 0)
        .unwrap()
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
    let s1 = g
        .start_session("builder", "fix slow checkout query")
        .unwrap();
    g.add_observation(&s1, "EXPLAIN shows seq scan on orders_archive")
        .unwrap();
    g.add_attempt(&s1, "added index on orders.user_id", false)
        .unwrap();
    g.end_session(&s1, "index hint did not help", vec![])
        .unwrap();

    // Session 2: success, promoted into the Codex with provenance.
    let s2 = g
        .start_session("builder", "rewrite checkout query")
        .unwrap();
    g.add_attempt(&s2, "rewrote query to use orders_archive_idx", true)
        .unwrap();
    let report = g
        .end_session(
            &s2,
            "rewrote checkout query successfully",
            vec![Promotion {
                kind: EntryKind::Decision,
                summary: "checkout queries must use orders_archive_idx".into(),
                body: "seq scan on orders_archive was the bottleneck".into(),
                review_of: None,
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
    let mut f1 = Entry::new(
        "ops",
        EntryKind::Decision,
        "feature flags live in launchdarkly",
    );
    f1.timestamp = at(20);
    codex.create(&f1).unwrap();
    let mut verified = f1.clone();
    verified.verify("ops-lead", None);
    verified.verification_log[0].timestamp = at(30);
    codex
        .update_with(&verified, &|id| codex.exists(id))
        .unwrap();
    let mut rolled = verified;
    rolled.mark_rolled_back("ops");
    rolled.verification_log.last_mut().unwrap().timestamp = at(40);
    codex.update_with(&rolled, &|id| codex.exists(id)).unwrap();

    // Another agent's private journal must never leak into briefings.
    let s3 = g
        .start_session("scout", "investigate deploy timeouts")
        .unwrap();
    g.add_observation(&s3, "deploy pipeline timeout in github actions runners")
        .unwrap();
    g.end_session(&s3, "runner timeouts documented", vec![])
        .unwrap();

    // Unrelated verified fact far from every query below.
    let mut unrelated = Entry::new("hr", EntryKind::Decision, "meeting room hera seats eight");
    unrelated.body = "bookings through the calendar".into();
    codex.create(&unrelated).unwrap();
    g.verify(unrelated.id, "office-manager", None).unwrap();

    Corpus {
        g,
        _dir: dir,
        checkout_decision: report.promoted[0],
    }
}

struct Score {
    ability: &'static str,
    passed: usize,
    total: usize,
}

impl Score {
    fn new(ability: &'static str) -> Self {
        Self {
            ability,
            passed: 0,
            total: 0,
        }
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
    let scores = vec![
        extraction(&c),
        multi_session(&c),
        knowledge_updates(&c),
        temporal(&c),
        abstention(&c),
        paraphrase(),
    ];

    let total: usize = scores.iter().map(|s| s.total).sum();
    let passed: usize = scores.iter().map(|s| s.passed).sum();
    println!("\ngungnir-bench: {passed}/{total} checks pass");
    for s in &scores {
        println!("  {:<26} {}/{}", s.ability, s.passed, s.total);
    }
    assert_eq!(
        passed, total,
        "bench regressions detected; see FAIL lines above"
    );
}

fn extraction(c: &Corpus) -> Score {
    let mut s = Score::new("information extraction");
    let codex = c.g.codex().unwrap();
    let journal = c.g.journal("builder").unwrap();

    // The promoted decision is retrievable by its own content.
    let hits =
        c.g.recall_layer(
            gungnir::Layer::Codex,
            &Query::new("orders archive bottleneck", 5),
        )
        .unwrap();
    s.check(
        "promoted fact recalled",
        hits.first()
            .is_some_and(|h| h.entry.summary.contains("orders_archive")),
    );

    // Provenance survived promotion: evidence resolves to the journal entry.
    let d1 = codex.require(c.checkout_decision).unwrap();
    let evidence_ok = match d1.evidence.first() {
        Some(gungnir::Evidence::Ref { id }) => journal.exists(*id).unwrap_or(false),
        _ => false,
    };
    s.check("promotion links to journal source", evidence_ok);

    // Archived transcript detail is searchable in the journal.
    let obs =
        c.g.recall_layer(
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
    s.check(
        "failed attempt present",
        b.markdown.contains("index hint did not help"),
    );
    s.check(
        "successful rewrite present",
        b.markdown.contains("rewrote checkout query"),
    );

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

    let current =
        c.g.search_layer(
            gungnir::Layer::Codex,
            &Query::new("budget reviews", 10).current(),
        )
        .unwrap();
    s.check(
        "current view keeps chain head only",
        current.hits.len() == 1 && current.hits[0].entry.summary.contains("q1"),
    );
    s.check(
        "superseded tail counted as hidden",
        current.coverage.hidden_superseded == 1,
    );

    // Honest coverage: the head is unverified even though its ancestor was
    // verified. Abstention reflects what is true now, not history.
    s.check(
        "head unverified reported honestly",
        current.coverage.unverified == 1 && current.coverage.verified == 0,
    );

    // Plain recall shows both generations with the stale one flagged.
    let b = c.g.brief("ops", "budget reviews", 10).unwrap();
    s.check(
        "stale generation flagged superseded",
        b.markdown.contains(", superseded]"),
    );

    s
}

fn temporal(c: &Corpus) -> Score {
    let mut s = Score::new("temporal reasoning");

    // Before the revision exists, the old fact is the only candidate.
    let early =
        c.g.search_layer(
            gungnir::Layer::Codex,
            &Query::new("budget reviews", 10).as_of(at(50)).current(),
        )
        .unwrap();
    s.check(
        "point-in-time before revision",
        early.hits.len() == 1 && early.hits[0].entry.summary.contains("q3"),
    );

    // After it lands, the head moves.
    let late =
        c.g.search_layer(
            gungnir::Layer::Codex,
            &Query::new("budget reviews", 10).as_of(at(150)).current(),
        )
        .unwrap();
    s.check(
        "point-in-time after revision",
        late.hits.len() == 1 && late.hits[0].entry.summary.contains("q1"),
    );

    // Rollback visibility flips at the recorded time, not at write time.
    let flag_before =
        c.g.search_layer(
            gungnir::Layer::Codex,
            &Query::new("feature flags", 10).as_of(at(35)),
        )
        .unwrap();
    s.check(
        "fact verified as-of before rollback",
        flag_before.coverage.verified == 1 && flag_before.hits.len() == 1,
    );
    let flag_after =
        c.g.search_layer(
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

    let nothing =
        c.g.search_layer(
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
    // none are verified instead of staying silent about it. This corpus has
    // nothing matching "cache tuning", so the verdict names the miss.
    let b = c.g.brief("builder", "cache tuning internals", 8).unwrap();
    s.check(
        "abstention line when nothing matched",
        b.markdown.contains("No knowledge matched this task"),
    );

    s
}

// ---------------------------------------------------------------------------
// Paraphrase recall: same corpus, three retrieval methods.
//
// legacy  — the pre-P1 scorer (boolean token presence, no stemming, no IDF),
//           reimplemented here so the regression is measured, not asserted.
// bm25    — shipped keyword path (BM25-lite + stemmer).
// hybrid  — BM25-lite fused with a deterministic SynonymEmbedder through
//           `search_layer_semantic`.
//
// The embedder is a test double, not a real model: it maps words onto
// synonym-cluster basis vectors, so its cosine is a stand-in for semantic
// similarity, not a measurement of any shipped model's quality. What it
// DOES measure is the machinery end to end — temporal filter, fusion,
// coverage, abstention — which is what this change ships.
// ---------------------------------------------------------------------------

/// Words grouped by meaning. Any word in a cluster lights that basis
/// dimension; two texts sharing a cluster get positive cosine even with
/// zero shared tokens.
const CLUSTERS: &[&[&str]] = &[
    &["postgres", "database", "db", "sql"],
    &["connection", "connections", "pool", "pooling", "pgbouncer"],
    &[
        "exhaustion",
        "exhausted",
        "saturation",
        "leak",
        "leaks",
        "limit",
        "limits",
    ],
    &[
        "migrate",
        "migrating",
        "migration",
        "moved",
        "move",
        "switch",
    ],
    &["session", "sessions", "storage"],
    &[
        "deploy",
        "deployment",
        "release",
        "releases",
        "rollout",
        "ship",
        "shipping",
    ],
    &["flag", "flags", "gated", "toggle", "feature"],
    &["auth", "oauth", "login", "authentication", "sso"],
    &["index", "indexes", "idx", "ordering"],
    &["slow", "latency", "bottleneck", "performance"],
    &["checkout", "payment", "cart"],
    &[
        "cache",
        "redis",
        "memcached",
        "caching",
        "eviction",
        "memory",
    ],
    &["budget", "finance", "reviews"],
];

struct SynonymEmbedder;

impl gungnir::embedding::Embedder for SynonymEmbedder {
    fn model(&self) -> &str {
        "synonym-test-double"
    }
    fn embed(&self, texts: &[String]) -> gungnir::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| {
                let lower = t.to_lowercase();
                let words: Vec<&str> = lower
                    .split(|c: char| !c.is_alphanumeric())
                    .filter(|w| w.len() > 1)
                    .collect();
                CLUSTERS
                    .iter()
                    .map(|cluster| {
                        if words
                            .iter()
                            .any(|w| cluster.iter().any(|cw| w == cw || w.starts_with(cw)))
                        {
                            1.0
                        } else {
                            0.0
                        }
                    })
                    .collect()
            })
            .collect())
    }
}

/// The scorer recall.rs shipped before this change: unique query tokens,
/// boolean presence (summary ×2, body ×1), normalized by token count.
fn legacy_hit(entry: &gungnir::Entry, query_tokens: &[&str]) -> bool {
    let summary = entry.summary.to_lowercase();
    let body = entry.body.to_lowercase();
    let contains =
        |text: &str, qt: &str| text.split(|c: char| !c.is_alphanumeric()).any(|t| t == qt);
    let mut raw = 0.0;
    for qt in query_tokens {
        if contains(&summary, qt) {
            raw += 2.0;
        } else if contains(&body, qt) {
            raw += 1.0;
        }
    }
    raw / query_tokens.len() as f64 > 0.0
}

/// A paraphrase query plus the summary substring identifying its target.
struct ParaCase {
    query: &'static str,
    /// What the writer called it — the vocabulary gap is the point.
    target: &'static str,
    /// legacy scorer expectation, computed by hand from vocab overlap.
    legacy_would_miss: bool,
}

fn paraphrase() -> Score {
    let mut s = Score::new("paraphrase recall");
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();

    let entries = [
        (
            "pgbouncer tunes pooling for postgres",
            "pool sized to 50 clients",
        ),
        (
            "migrating sessions from mysql to postgres",
            "cutover ran clean",
        ),
        (
            "feature flag rollout via launchdarkly",
            "gradual releases only",
        ),
        ("oauth login moved to sso", "single sign on enforced"),
        (
            "redis eviction policy for session cache",
            "lru capped at 2gb",
        ),
        (
            "index on orders table removed seq scan",
            "query time 800ms to 12ms",
        ),
        ("budget reviews moved to q1", "finance calendar updated"),
        // Noise entries whose vocabulary touches no query cluster.
        (
            "meeting room hera seats eight",
            "bookings through the calendar",
        ),
        ("parking pass renewal", "forms on the intranet"),
    ];
    for (i, (summary, body)) in entries.iter().enumerate() {
        let mut e = Entry::new("ops", EntryKind::Decision, *summary);
        e.body = (*body).into();
        codex.create(&e).unwrap();
        // Verify every other entry so coverage has both buckets present.
        if i % 2 == 0 {
            g.verify(e.id, "ops-lead", None).unwrap();
        }
    }

    let cases = [
        // Vocabulary-disjoint: only the vector channel can find these.
        ParaCase {
            query: "database connection limits",
            target: "pgbouncer",
            legacy_would_miss: true,
        },
        ParaCase {
            query: "deploying toggled functionality",
            target: "launchdarkly",
            legacy_would_miss: true,
        },
        ParaCase {
            query: "authentication flow",
            target: "oauth login",
            legacy_would_miss: true,
        },
        // Inflection gap: shared roots under different word forms — the
        // stemmer bridges these, boolean tokens cannot.
        ParaCase {
            query: "session storage migration",
            target: "migrating sessions",
            legacy_would_miss: true,
        },
        ParaCase {
            query: "ordering indexes",
            target: "index on orders",
            legacy_would_miss: true,
        },
        // Shared vocabulary: everyone finds these.
        ParaCase {
            query: "session cache evictions",
            target: "redis eviction",
            legacy_would_miss: false,
        },
        ParaCase {
            query: "budget reviews",
            target: "budget reviews",
            legacy_would_miss: false,
        },
    ];

    let embedder = gungnir::embedding::CachedEmbedder::new(SynonymEmbedder, dir.path());
    let store = g.codex().unwrap();
    let all = store.entries().unwrap();

    let mut legacy_hits = 0;
    let mut bm25_hits = 0;
    let mut hybrid_hits = 0;
    let mut false_abstentions = 0;

    println!(
        "\nparaphrase suite ({} queries, {} entries)",
        cases.len(),
        all.len()
    );
    for case in &cases {
        // legacy
        let qtokens: Vec<&str> = case
            .query
            .split_whitespace()
            .filter(|t| t.len() > 1)
            .collect();
        let legacy_found = all
            .iter()
            .any(|e| legacy_hit(e, &qtokens) && e.summary.contains(case.target));
        if legacy_found {
            legacy_hits += 1;
        }

        // bm25 (shipped keyword path)
        let q = Query::new(case.query, 10);
        let kw = g.search_layer(gungnir::Layer::Codex, &q).unwrap();
        let kw_found = kw
            .hits
            .iter()
            .any(|h| h.entry.summary.contains(case.target));
        if kw_found {
            bm25_hits += 1;
        }

        // hybrid
        let hy = g
            .search_layer_semantic(gungnir::Layer::Codex, &q, &embedder)
            .unwrap();
        let hy_found = hy
            .hits
            .iter()
            .any(|h| h.entry.summary.contains(case.target));
        if hy_found {
            hybrid_hits += 1;
        }

        // False abstention: the target exists in the corpus, so the
        // coverage line must never claim none — whether or not the hit
        // list found it. An empty-coverage miss is the dangerous direction.
        if hy.coverage.total_visible() == 0 {
            false_abstentions += 1;
        }

        // Ordering, not just inclusion: the fused rank has to put the
        // target first in its verification-state bucket. `hits.contains`
        // would stay green if fusion stopped ordering at all — inclusion
        // comes from channel scores, not the fused rank.
        if hy_found {
            let target_state = all
                .iter()
                .find(|e| e.summary.contains(case.target))
                .unwrap()
                .verification
                .clone();
            let first_in_bucket = hy
                .hits
                .iter()
                .find(|h| h.entry.verification == target_state)
                .unwrap();
            s.check(
                &format!(
                    "hybrid ranks target first in its bucket for '{}'",
                    case.query
                ),
                first_in_bucket.entry.summary.contains(case.target),
            );
        }

        println!(
            "  {:<32} legacy={} bm25={} hybrid={} cov={}",
            case.query,
            legacy_found as u8,
            kw_found as u8,
            hy_found as u8,
            hy.coverage.total_visible()
        );
        // The fixture's premise must hold: vocabulary the legacy scorer
        // cannot bridge really is absent for it, otherwise the suite
        // measures nothing.
        s.check(
            &format!("legacy gap exists for '{}'", case.query),
            legacy_found == !case.legacy_would_miss,
        );
        s.check(&format!("hybrid finds '{}'", case.query), hy_found);
    }

    println!(
        "\nrecall@1-of-target over {} paraphrase queries: legacy={} bm25={} hybrid={}",
        cases.len(),
        legacy_hits,
        bm25_hits,
        hybrid_hits
    );

    s.check(
        "bm25-lite beats the legacy boolean scorer",
        bm25_hits > legacy_hits,
    );
    s.check(
        "hybrid beats bm25-lite on paraphrases",
        hybrid_hits > bm25_hits,
    );
    s.check(
        "hybrid finds every paraphrase target",
        hybrid_hits == cases.len(),
    );
    s.check(
        "no false abstention when knowledge exists under other words",
        false_abstentions == 0,
    );

    s
}
