//! Reference runner for gungnir-bench (see `bench/README.md` for the spec).
//!
//! The corpus and the checks are data: `bench/corpus.yaml` describes the
//! world to load, `bench/checks.yaml` the exact-match assertions over it,
//! grouped by LongMemEval's five abilities (information extraction,
//! multi-session reasoning, knowledge updates, temporal reasoning,
//! abstention). Grading is exact and offline; scores print per ability with
//! `cargo test --test bench -- --nocapture`.
//!
//! The `paraphrase` suite below stays in Rust: it measures retrieval
//! machinery (BM25-lite vs hybrid fusion), not a store capability the data
//! files express. It runs beside the YAML suite under the same bench.
//!
//! This is a self-benchmark, not LongMemEval. Vendor-published numbers from
//! other systems are not comparable to it.

use std::collections::HashMap;

use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;

use gungnir::{Entry, EntryKind, Evidence, Gungnir, Layer, Promotion, Query, Store};

// ---------------------------------------------------------------------------
// Corpus and check files
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFile {
    epoch_base: i64,
    sessions: Vec<CorpusSession>,
    facts: Vec<CorpusFact>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusSession {
    agent: String,
    task: String,
    transcript: Vec<CorpusLine>,
    journal_summary: String,
    #[serde(default)]
    promote: Vec<CorpusPromotion>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusLine {
    kind: String,
    text: String,
    #[serde(default)]
    outcome: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusPromotion {
    label: String,
    kind: EntryKind,
    summary: String,
    body: String,
    verify_as: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusFact {
    label: String,
    agent: String,
    kind: EntryKind,
    summary: String,
    #[serde(default)]
    body: String,
    at_hours: Option<i64>,
    revises: Option<String>,
    verify_as: Option<String>,
    verify_at: Option<i64>,
    mark_rolled_back_as: Option<String>,
    mark_rolled_back_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksFile {
    #[allow(dead_code)]
    version: u32,
    abilities: Vec<Ability>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ability {
    name: String,
    checks: Vec<Check>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    name: String,
    run: Run,
    expect: Expect,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Run {
    search: Option<String>,
    brief: Option<String>,
    entry: Option<String>,
    layer: Option<String>,
    agent: Option<String>,
    limit: Option<usize>,
    #[serde(default)]
    current: bool,
    as_of: Option<i64>,
}

/// Exact-match assertions. Each present field is one independent check;
/// `None` means "assert nothing about this". A check YAML entry may carry
/// several fields; the runner evaluates them as one unit against the same
/// run output.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Expect {
    hits_exactly: Option<usize>,
    hits_at_least: Option<usize>,
    hit0_summary_contains: Option<String>,
    codex_hit0_summary_contains: Option<String>,
    markdown_contains: Option<String>,
    markdown_not_contains: Option<String>,
    evidence_ref_resolves_in: Option<String>,
    verified: Option<usize>,
    unverified: Option<usize>,
    hidden_superseded: Option<usize>,
    hidden_rolled_back: Option<usize>,
    total_visible: Option<usize>,
}

// ---------------------------------------------------------------------------
// Built corpus
// ---------------------------------------------------------------------------

/// The store plus the label → id map from corpus.yaml and the epoch clock.
struct Corpus {
    g: Gungnir,
    /// Holds the tempdir open for as long as the facade lives.
    _dir: tempfile::TempDir,
    labels: HashMap<String, gungnir::EntryId>,
    epoch_base: i64,
}

impl Corpus {
    fn at(&self, hours: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(self.epoch_base + hours * 3600, 0)
            .unwrap()
    }

    fn layer_store(&self, name: &str) -> Store {
        if name == "codex" {
            self.g.codex().unwrap()
        } else if let Some(agent) = name
            .strip_prefix("journal(")
            .and_then(|s| s.strip_suffix(')'))
        {
            self.g.journal(agent).unwrap()
        } else {
            panic!("unknown layer '{name}'")
        }
    }
}

fn load_corpus() -> Corpus {
    let raw: CorpusFile = serde_yaml::from_str(include_str!("../bench/corpus.yaml")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let mut corpus = Corpus {
        g,
        _dir: dir,
        labels: HashMap::new(),
        epoch_base: raw.epoch_base,
    };

    for s in &raw.sessions {
        let session = corpus.g.start_session(&s.agent, &s.task).unwrap();
        for line in &s.transcript {
            match line.kind.as_str() {
                "observation" => {
                    corpus.g.add_observation(&session, &line.text).unwrap();
                }
                "attempt" => {
                    corpus
                        .g
                        .add_attempt(
                            &session,
                            &line.text,
                            line.outcome.as_deref() == Some("succeeded"),
                        )
                        .unwrap();
                }
                other => panic!("unknown transcript kind '{other}'"),
            }
        }
        let report = corpus
            .g
            .end_session(
                &session,
                &s.journal_summary,
                s.promote
                    .iter()
                    .map(|p| Promotion {
                        kind: p.kind,
                        summary: p.summary.clone(),
                        body: p.body.clone(),
                        review_of: None,
                    })
                    .collect(),
            )
            .unwrap();
        for (p, id) in s.promote.iter().zip(&report.promoted) {
            corpus.labels.insert(p.label.clone(), *id);
            if let Some(verifier) = &p.verify_as {
                corpus.g.verify(*id, verifier, None).unwrap();
            }
        }
    }

    let codex = corpus.g.codex().unwrap();
    for f in &raw.facts {
        let mut e = Entry::new(&f.agent, f.kind, &f.summary);
        e.body = f.body.clone();
        if let Some(h) = f.at_hours {
            e.timestamp = corpus.at(h);
        }
        if let Some(tail) = &f.revises {
            e.revises = Some(corpus.labels[tail]);
        }
        codex.create(&e).unwrap();
        corpus.labels.insert(f.label.clone(), e.id);

        // Timestamped transitions stamp the log record by hand so as-of
        // checks interrogate recorded history, not ingest order.
        if let Some(verifier) = &f.verify_as {
            let mut v = e.clone();
            v.verify(verifier, None);
            if let Some(h) = f.verify_at {
                v.verification_log[0].timestamp = corpus.at(h);
            }
            codex.update_with(&v, &|id| codex.exists(id)).unwrap();
        }
        if let Some(agent) = &f.mark_rolled_back_as {
            let mut r = codex.require(e.id).unwrap();
            r.mark_rolled_back(agent);
            if let Some(h) = f.mark_rolled_back_at {
                r.verification_log.last_mut().unwrap().timestamp = corpus.at(h);
            }
            codex.update_with(&r, &|id| codex.exists(id)).unwrap();
        }
    }
    corpus
}

// ---------------------------------------------------------------------------
// Check execution
// ---------------------------------------------------------------------------

struct Score {
    ability: String,
    passed: usize,
    total: usize,
}

impl Score {
    fn new(ability: &str) -> Self {
        Self {
            ability: ability.to_string(),
            passed: 0,
            total: 0,
        }
    }
    fn check(&mut self, name: &str, ok: bool) {
        self.total += 1;
        if ok {
            self.passed += 1;
        } else {
            println!("  FAIL {name}");
        }
    }
}

fn run_checks(c: &Corpus) -> Vec<Score> {
    let file: ChecksFile = serde_yaml::from_str(include_str!("../bench/checks.yaml")).unwrap();
    file.abilities
        .iter()
        .map(|a| {
            let mut s = Score::new(&a.name);
            for check in &a.checks {
                s.check(&check.name, eval(c, check));
            }
            s
        })
        .collect()
}

/// Evaluate one check: dispatch on whichever run shape is set.
fn eval(c: &Corpus, check: &Check) -> bool {
    if check.run.search.is_some() {
        eval_search(c, check)
    } else if check.run.brief.is_some() {
        eval_brief(c, check)
    } else if check.run.entry.is_some() {
        eval_entry(c, check)
    } else {
        panic!("check '{}' has no run shape", check.name)
    }
}

fn eval_search(c: &Corpus, check: &Check) -> bool {
    let run = &check.run;
    let mut q = Query::new(run.search.as_deref().unwrap(), run.limit.unwrap_or(10));
    if run.current {
        q = q.current();
    }
    if let Some(h) = run.as_of {
        q = q.as_of(c.at(h));
    }
    let layer = match run.layer.as_deref().unwrap_or("codex") {
        "codex" => Layer::Codex,
        name if name.starts_with("journal(") && name.ends_with(')') => Layer::Journal {
            agent: &name[8..name.len() - 1],
        },
        other => panic!("unknown layer '{other}'"),
    };
    let out = c.g.search_layer(layer, &q).unwrap();

    let e = &check.expect;
    field(e.hits_exactly, |v| out.hits.len() == v)
        && field(e.hits_at_least, |v| out.hits.len() >= v)
        && field(e.hit0_summary_contains.as_deref(), |v| {
            out.hits
                .first()
                .is_some_and(|h| h.entry.summary.contains(v))
        })
        && field(e.verified, |v| out.coverage.verified == v)
        && field(e.unverified, |v| out.coverage.unverified == v)
        && field(e.hidden_superseded, |v| out.coverage.hidden_superseded == v)
        && field(e.hidden_rolled_back, |v| {
            out.coverage.hidden_rolled_back == v
        })
        && field(e.total_visible, |v| out.coverage.total_visible() == v)
}

fn eval_brief(c: &Corpus, check: &Check) -> bool {
    let run = &check.run;
    let b =
        c.g.brief(
            run.agent.as_deref().expect("brief run needs agent"),
            run.brief.as_deref().unwrap(),
            run.limit.unwrap_or(10),
        )
        .unwrap();
    let e = &check.expect;
    field(e.markdown_contains.as_deref(), |v| b.markdown.contains(v))
        && field(e.markdown_not_contains.as_deref(), |v| {
            !b.markdown.contains(v)
        })
        && field(e.codex_hit0_summary_contains.as_deref(), |v| {
            b.codex_hits
                .first()
                .is_some_and(|h| h.entry.summary.contains(v))
        })
}

fn eval_entry(c: &Corpus, check: &Check) -> bool {
    let run = &check.run;
    let e = &check.expect;
    field(e.evidence_ref_resolves_in.as_deref(), |layer| {
        let id = c.labels[run.entry.as_deref().unwrap()];
        let entry = c.g.codex().unwrap().require(id).unwrap();
        match entry.evidence.first() {
            Some(Evidence::Ref { id: ev }) => c.layer_store(layer).exists(*ev).unwrap_or(false),
            _ => false,
        }
    })
}

/// Assert `want` against `actual` when the expect field is set; an unset
/// field always passes.
fn field<T>(expect: Option<T>, f: impl FnOnce(T) -> bool) -> bool {
    match expect {
        Some(want) => f(want),
        None => true,
    }
}

/// The check format is strict: a misspelled assertion field fails to
/// parse instead of silently passing without testing anything.
#[test]
fn checks_yaml_rejects_unknown_fields() {
    let yaml = r#"
version: 1
abilities:
  - name: typo test
    checks:
      - name: misspelled assertion
        run: {search: "budget", layer: codex}
        expect: {hits_exactlly: 1}
"#;
    // The file must parse far enough to hit `Expect`'s unknown-field
    // check, or the assertion passes on the missing version instead.
    let err = serde_yaml::from_str::<ChecksFile>(yaml)
        .err()
        .unwrap()
        .to_string();
    assert!(
        err.contains("hits_exactlly"),
        "error must name the unknown field, got: {err}"
    );
}

#[test]
fn gungnir_bench() {
    let c = load_corpus();
    let mut scores = run_checks(&c);
    scores.push(paraphrase());

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

/// Peer coverage through the vector channel: a session archived under
/// synonyms, not the query's tokens, still counts for a semantic briefing.
#[test]
fn peer_coverage_semantic_counts_paraphrased_peer_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();

    let scout = g.start_session("scout", "investigate db issues").unwrap();
    g.add_observation(&scout, "postgres pool exhausted during deploy")
        .unwrap();
    g.end_session(&scout, "pool exhaustion diagnosed", vec![])
        .unwrap();

    // Zero keyword overlap between the topic and the session text.
    let kw = g
        .peer_coverage(None, "database connection saturation")
        .unwrap();
    assert_eq!(kw.agents_attempted, 0);

    let embedder = gungnir::embedding::CachedEmbedder::new(SynonymEmbedder, dir.path());
    let sem = g
        .peer_coverage_semantic(None, "database connection saturation", &embedder)
        .unwrap();
    assert_eq!(sem.agents_attempted, 1);
    assert_eq!(sem.entries, 1);
}
