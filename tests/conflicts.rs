//! Conflict-candidate loop end to end: a write beside a live verified fact
//! records a candidate, the queue survives restarts, resolution verbs close
//! it through the existing trust transitions, and briefings warn while the
//! loop is open.

use gungnir::{
    ConflictStatus, ConflictVerb, Entry, EntryId, EntryKind, Gungnir, Layer, Query,
    VerificationState,
};

fn gng() -> (tempfile::TempDir, Gungnir) {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    (dir, g)
}

fn write_verified(g: &Gungnir, summary: &str) -> Entry {
    let mut e = Entry::new("a", EntryKind::Decision, summary);
    e.verify("review", None);
    let id = e.id;
    g.add(e, Layer::Codex).unwrap();
    g.codex().unwrap().require(id).unwrap()
}

fn add_entry(g: &Gungnir, summary: &str) -> gungnir::WriteOutcome {
    g.add(Entry::new("a", EntryKind::Decision, summary), Layer::Codex)
        .unwrap()
}

#[test]
fn write_next_to_verified_fact_records_open_candidate() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");

    let out = add_entry(&g, "sessions use postgres");
    assert_eq!(out.conflicts.len(), 1);
    let c = &out.conflicts[0];
    assert_eq!(c.new_entry, out.id);
    assert_eq!(c.conflicting_entry, old.id);
    assert_eq!(c.status, ConflictStatus::Open);
    assert_eq!(c.layer, "codex");

    // Advisory only: the write landed despite the disagreement.
    assert!(g.codex().unwrap().exists(out.id).unwrap());
    assert_eq!(g.open_conflicts().unwrap().len(), 1);
}

#[test]
fn non_conflicting_write_records_nothing() {
    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");

    // Unrelated topic: no candidate.
    let out = add_entry(&g, "kubernetes ingress is slow");
    assert!(out.conflicts.is_empty());

    // Restating the same claim is corroboration, not conflict.
    let out = add_entry(&g, "sessions use mysql");
    assert!(out.conflicts.is_empty());
    assert!(g.open_conflicts().unwrap().is_empty());
}

#[test]
fn resolve_by_supersede_links_the_revision_chain() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    let cid = out.conflicts[0].id;

    let rec = g
        .resolve_conflict(cid, ConflictVerb::Supersede, "ops")
        .unwrap();
    assert_eq!(rec.status, ConflictStatus::ResolvedSuperseded);
    assert_eq!(rec.resolved_by.as_deref(), Some("ops"));

    // The new claim now revises the old one: current views hide the old
    // fact, and the old keeps its verified state (superseded ≠ wrong).
    let codex = g.codex().unwrap();
    let new_entry = codex.require(out.id).unwrap();
    assert_eq!(new_entry.revises, Some(old.id));
    assert_eq!(
        codex.require(old.id).unwrap().verification,
        VerificationState::Verified
    );
    let view = g
        .search_layer(Layer::Codex, &Query::new("sessions use", 10).current())
        .unwrap();
    assert_eq!(view.hits.len(), 1);
    assert_eq!(view.hits[0].entry.id, out.id);
}

#[test]
fn resolve_by_contradict_marks_the_verified_fact() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    let cid = out.conflicts[0].id;

    let rec = g
        .resolve_conflict(cid, ConflictVerb::Contradict, "ops")
        .unwrap();
    assert_eq!(rec.status, ConflictStatus::ResolvedContradicted);

    // Logged through the same transition `contradict` always used.
    let stored = g.codex().unwrap().require(old.id).unwrap();
    assert_eq!(
        stored.verification,
        VerificationState::Contradicted { by: out.id }
    );
    assert_eq!(
        stored.verification_log.last().unwrap().status,
        gungnir::RecordStatus::Contradicted
    );
}

#[test]
fn resolve_by_dismiss_closes_without_touching_entries() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    let cid = out.conflicts[0].id;

    g.resolve_conflict(cid, ConflictVerb::Dismiss, "ops")
        .unwrap();
    assert!(g.open_conflicts().unwrap().is_empty());
    assert_eq!(
        g.codex().unwrap().require(old.id).unwrap().verification,
        VerificationState::Verified
    );
    assert!(g
        .codex()
        .unwrap()
        .require(out.id)
        .unwrap()
        .revises
        .is_none());

    // Resolved records stay in --all but cannot be resolved again.
    assert_eq!(g.all_conflicts().unwrap().len(), 1);
    let err = g
        .resolve_conflict(cid, ConflictVerb::Dismiss, "ops")
        .unwrap_err()
        .to_string();
    assert!(err.contains("already"), "{err}");
}

#[test]
fn candidates_survive_restart_as_markdown_files() {
    let (d, g) = gng();
    write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    let cid = out.conflicts[0].id;

    // The queue is files: one markdown record per candidate.
    let path = d.path().join(".conflicts").join(format!("{cid}.md"));
    assert!(path.exists());
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("sessions use postgres"));
    assert!(raw.contains("sessions use mysql"));

    // A fresh open (process restart) sees the same open candidate.
    drop(g);
    let g2 = Gungnir::open(d.path()).unwrap();
    let open = g2.open_conflicts().unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].id, cid);
    assert_eq!(open[0].status, ConflictStatus::Open);
}

#[test]
fn journal_write_flags_verified_codex_fact() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");

    let out = g
        .add(
            Entry::new("scout", EntryKind::Observation, "sessions use postgres"),
            Layer::Journal { agent: "scout" },
        )
        .unwrap();
    assert_eq!(out.conflicts.len(), 1);
    // The codex is where the verified fact lives; the record says so.
    assert_eq!(out.conflicts[0].layer, "codex");
    assert_eq!(out.conflicts[0].conflicting_entry, old.id);
}

#[test]
fn supersede_resolution_refuses_cross_layer_links() {
    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");
    let out = g
        .add(
            Entry::new("scout", EntryKind::Observation, "sessions use postgres"),
            Layer::Journal { agent: "scout" },
        )
        .unwrap();
    let cid = out.conflicts[0].id;

    // A journal revision would not hide the codex parent under `current`
    // views, so the verb refuses rather than write a broken chain.
    let err = g
        .resolve_conflict(cid, ConflictVerb::Supersede, "ops")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot span layers"), "{err}");
    assert_eq!(g.open_conflicts().unwrap().len(), 1);

    // The same candidate resolves by contradict instead.
    g.resolve_conflict(cid, ConflictVerb::Contradict, "ops")
        .unwrap();
    assert!(g.open_conflicts().unwrap().is_empty());
}

#[test]
fn contradict_resolution_refuses_rolled_back_targets() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    let cid = out.conflicts[0].id;

    // The candidate went stale: the verified fact was rolled back between
    // detection and resolution.
    let codex = g.codex().unwrap();
    let mut stale = codex.require(old.id).unwrap();
    stale.mark_rolled_back("ops");
    codex.update(&stale).unwrap();
    let err = g
        .resolve_conflict(cid, ConflictVerb::Contradict, "ops")
        .unwrap_err()
        .to_string();
    assert!(err.contains("rolled back"), "{err}");
    assert_eq!(g.open_conflicts().unwrap().len(), 1);
}

#[test]
fn supersede_resolution_refuses_to_relink_an_existing_chain() {
    let (_d, g) = gng();
    let a = write_verified(&g, "sessions use mysql");
    let other = write_verified(&g, "sessions require auth tokens");

    // The new claim already revises a different entry.
    let out = g
        .add(
            {
                let mut e = Entry::new("a", EntryKind::Decision, "sessions use postgres");
                e.revises = Some(other.id);
                e
            },
            Layer::Codex,
        )
        .unwrap();
    let cid = out
        .conflicts
        .iter()
        .find(|c| c.conflicting_entry == a.id)
        .expect("candidate against the mysql fact")
        .id;
    let err = g
        .resolve_conflict(cid, ConflictVerb::Supersede, "ops")
        .unwrap_err()
        .to_string();
    assert!(err.contains("already revises"), "{err}");
}

#[test]
fn briefing_warns_while_a_candidate_is_open() {
    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");

    let b = g.brief("a", "sessions storage", 8).unwrap();
    assert!(
        b.markdown.contains("1 unresolved conflict candidate"),
        "{}",
        b.markdown
    );

    g.resolve_conflict(out.conflicts[0].id, ConflictVerb::Dismiss, "ops")
        .unwrap();
    let b = g.brief("a", "sessions storage", 8).unwrap();
    assert!(!b.markdown.contains("unresolved conflict"));
}

#[test]
fn end_session_promotions_raise_candidates() {
    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");

    let s = g.start_session("builder", "migrate sessions").unwrap();
    g.add_observation(&s, "postgres fits better").unwrap();
    let report = g
        .end_session(
            &s,
            "migrated sessions",
            vec![gungnir::Promotion {
                kind: EntryKind::Decision,
                summary: "sessions use postgres".into(),
                body: "benchmarked".into(),
                review_of: None,
            }],
        )
        .unwrap();
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(g.open_conflicts().unwrap().len(), 1);
}

#[test]
fn supersede_of_one_fact_still_flags_other_verified_conflicts() {
    let (_d, g) = gng();
    let v1 = write_verified(&g, "sessions use mysql");
    let other = write_verified(&g, "sessions require auth tokens");

    // The revision's own parent is exempt, but a second verified fact on
    // the same frame is not.
    let out = g
        .supersede(v1.id, "a", "sessions require refresh tokens", "new rule")
        .unwrap();
    assert!(
        out.conflicts
            .iter()
            .any(|c| c.conflicting_entry == other.id),
        "expected a candidate against the other verified fact"
    );
    assert!(
        !out.conflicts.iter().any(|c| c.conflicting_entry == v1.id),
        "supersession parent must not be flagged"
    );
}

#[test]
fn semantic_channel_catches_paraphrased_conflict() {
    use gungnir::embedding::Embedder;

    // Fake embedder: texts about sessions/storage get one vector, others a
    // different direction. Keyword tokens of "storage runs on postgres" and
    // "sessions use mysql" share nothing, so only the semantic channel can
    // raise this candidate.
    struct Fake;
    impl Embedder for Fake {
        fn model(&self) -> &str {
            "fake"
        }
        fn embed(&self, texts: &[String]) -> gungnir::Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.contains("session") || t.contains("storage") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        }
    }

    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");

    let out = g
        .add_semantic(
            Entry::new("a", EntryKind::Decision, "storage runs on postgres"),
            Layer::Codex,
            &Fake,
        )
        .unwrap();
    assert_eq!(out.conflicts.len(), 1);
    assert!(out.conflicts[0]
        .signals
        .contains(&gungnir::ConflictSignal::SemanticOverlap));
}

#[test]
fn contradict_is_exposed_and_logged() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");

    g.contradict(old.id, out.id, "ops", None).unwrap();
    let stored = g.codex().unwrap().require(old.id).unwrap();
    assert_eq!(
        stored.verification,
        VerificationState::Contradicted { by: out.id }
    );
}

#[test]
fn resolve_by_contradict_refuses_to_replace_an_earlier_refutation() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let first = add_entry(&g, "sessions use postgres");
    let second = add_entry(&g, "sessions use sqlite");
    assert_eq!(first.conflicts.len(), 1);
    assert_eq!(second.conflicts.len(), 1);

    g.resolve_conflict(first.conflicts[0].id, ConflictVerb::Contradict, "ops")
        .unwrap();

    // A second candidate against the same fact cannot silently swap the
    // contradicted_by link — same policy as supersede's relink refusal.
    let err = g
        .resolve_conflict(second.conflicts[0].id, ConflictVerb::Contradict, "ops")
        .unwrap_err();
    assert!(err.to_string().contains("already contradicted"));

    let codex = g.codex().unwrap();
    let entry = codex.require(old.id).unwrap();
    match entry.verification {
        VerificationState::Contradicted { by } => assert_eq!(by, first.id),
        ref other => panic!("expected contradicted, got {other:?}"),
    }
}

#[test]
fn write_still_lands_when_the_queue_cannot_record() {
    let (d, g) = gng();
    write_verified(&g, "sessions use mysql");
    // A regular file where the queue wants its directory: every record
    // call fails, and the write must still succeed (SPEC: advisory only).
    std::fs::write(d.path().join(".conflicts"), "not a directory").unwrap();

    let outcome = add_entry(&g, "sessions use postgres");
    assert!(outcome.conflicts.is_empty());
    assert!(!outcome.warnings.is_empty());
    assert!(g.codex().unwrap().exists(outcome.id).unwrap());
}

#[test]
fn conflicts_listing_reports_corrupt_records() {
    let (d, g) = gng();
    write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    let path = d
        .path()
        .join(".conflicts")
        .join(format!("{}.md", out.conflicts[0].id));
    std::fs::write(&path, "no frontmatter here").unwrap();

    // The audit surface must report corruption, not skip it; the lenient
    // read briefings use is what tolerates it.
    assert!(g.open_conflicts().is_err());
    assert!(g.all_conflicts().is_err());
    assert!(g.open_conflicts_lenient().unwrap().is_empty());
}

#[test]
fn semantic_restatement_is_corroboration() {
    use gungnir::embedding::Embedder;

    // Every text embeds to the same vector: cosine 1.0 for any pair, so a
    // verbatim restatement would flag unless the identical-token guard
    // skips it the way the keyword channel does.
    struct Constant;
    impl Embedder for Constant {
        fn model(&self) -> &str {
            "constant"
        }
        fn embed(&self, texts: &[String]) -> gungnir::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");

    let out = g
        .add_semantic(
            Entry::new("a", EntryKind::Decision, "sessions use mysql"),
            Layer::Codex,
            &Constant,
        )
        .unwrap();
    assert!(out.conflicts.is_empty());
    assert!(g.open_conflicts().unwrap().is_empty());
}

#[test]
fn semantic_candidates_are_bounded() {
    use gungnir::embedding::Embedder;

    struct Constant;
    impl Embedder for Constant {
        fn model(&self) -> &str {
            "constant"
        }
        fn embed(&self, texts: &[String]) -> gungnir::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    let (_d, g) = gng();
    // 70 verified facts on a topic the new claim shares no tokens with:
    // only the semantic channel can flag them, and the pool is unbounded.
    for i in 0..70 {
        write_verified(&g, &format!("retention policy variant {i} keeps journals"));
    }

    let out = g
        .add_semantic(
            Entry::new("a", EntryKind::Decision, "cache backend uses memcached"),
            Layer::Codex,
            &Constant,
        )
        .unwrap();
    assert_eq!(out.conflicts.len(), 64);
}

#[test]
fn semantic_channel_flags_same_summary_when_bodies_disagree() {
    use gungnir::embedding::Embedder;

    struct Constant;
    impl Embedder for Constant {
        fn model(&self) -> &str {
            "constant"
        }
        fn embed(&self, texts: &[String]) -> gungnir::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    let (_d, g) = gng();
    let mut old = Entry::new("a", EntryKind::Decision, "sessions use mysql");
    old.body = "production runs mysql".into();
    old.verify("review", None);
    g.add(old.clone(), Layer::Codex).unwrap();

    // Same summary, disagreeing body: corroboration is judged over the
    // full embedded text, so this still flags as a candidate.
    let mut e = Entry::new("a", EntryKind::Decision, "sessions use mysql");
    e.body = "production no longer runs mysql".into();
    let out = g.add_semantic(e, Layer::Codex, &Constant).unwrap();
    assert_eq!(out.conflicts.len(), 1);
    assert!(out.conflicts[0]
        .signals
        .contains(&gungnir::ConflictSignal::SemanticOverlap));
}

#[test]
fn briefing_warns_on_candidates_whose_entries_left_the_hits() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    assert_eq!(out.conflicts.len(), 1);

    // Supersede both endpoints behind off-topic revisions: under the
    // current view neither entry can be a briefing hit anymore, so only
    // the candidate's recorded summaries still match the task — the
    // warning must come from the snapshots, not the hit list.
    g.supersede(old.id, "a", "vacuum cadence weekly", "")
        .unwrap();
    g.supersede(out.id, "a", "vacuum cadence daily", "")
        .unwrap();

    let briefing = g.brief("a", "sessions", 5).unwrap();
    let text = format!("{briefing:?}");
    assert!(text.contains("conflict"), "briefing must warn: {text}");
}

#[test]
fn briefing_hides_other_agents_journal_conflicts() {
    use gungnir::gungnir::Layer;

    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");

    // Agent B writes a disputing claim in *its own* journal: the candidate
    // records codex as the layer (the scan ran against codex) but the new
    // claim is private to B.
    let b = g
        .add(
            Entry::new("b", EntryKind::Decision, "sessions use postgres"),
            Layer::Journal { agent: "b" },
        )
        .unwrap();
    assert_eq!(b.conflicts.len(), 1);
    assert_eq!(b.conflicts[0].layer, "codex");

    // B's briefing warns — it can see its own journal claim.
    let mine = format!("{:?}", g.brief("b", "sessions", 5).unwrap());
    assert!(mine.contains("conflict"), "b should warn: {mine}");

    // A's briefing on the same topic must not count B's private
    // candidate — that would leak B's journal activity.
    let theirs = format!("{:?}", g.brief("a", "sessions", 5).unwrap());
    assert!(!theirs.contains("conflict"), "a must not warn: {theirs}");
}

#[test]
fn acknowledgment_words_are_not_negations() {
    let (_d, g) = gng();
    write_verified(&g, "migration is slow");

    // "noted" stems to `not`; decided on raw tokens it is an
    // acknowledgment, not a negation, so no candidate is recorded.
    let out = add_entry(&g, "noted migration is slow");
    assert!(out.conflicts.is_empty());
    assert!(g.open_conflicts().unwrap().is_empty());
}

#[test]
fn direct_contradict_refuses_to_replace_an_earlier_refutation() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let first = add_entry(&g, "sessions use postgres");
    let second = add_entry(&g, "sessions use sqlite");

    g.contradict(old.id, first.id, "ops", None).unwrap();

    // The bare verb now shares the resolve path's refutation policy: a
    // second contradict cannot swap contradicted_by.
    let err = g.contradict(old.id, second.id, "ops", None).unwrap_err();
    assert!(err.to_string().contains("already contradicted"), "{err}");

    let entry = g.codex().unwrap().require(old.id).unwrap();
    assert_eq!(
        entry.verification,
        VerificationState::Contradicted { by: first.id }
    );
}

#[test]
fn direct_contradict_refuses_rolled_back_targets() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");

    let codex = g.codex().unwrap();
    let mut stale = codex.require(old.id).unwrap();
    stale.mark_rolled_back("ops");
    codex.update(&stale).unwrap();

    let err = g.contradict(old.id, out.id, "ops", None).unwrap_err();
    assert!(err.to_string().contains("rolled back"), "{err}");
}

#[test]
fn end_session_reports_warnings_when_candidates_cannot_record() {
    let (d, g) = gng();
    write_verified(&g, "sessions use mysql");
    // A regular file where the queue wants its directory: every record
    // call fails, and the report must surface that as warnings.
    std::fs::write(d.path().join(".conflicts"), "not a directory").unwrap();

    let s = g.start_session("builder", "migrate sessions").unwrap();
    let report = g
        .end_session(
            &s,
            "migrated sessions",
            vec![gungnir::Promotion {
                kind: EntryKind::Decision,
                summary: "sessions use postgres".into(),
                body: "benchmarked".into(),
                review_of: None,
            }],
        )
        .unwrap();
    assert!(report.conflicts.is_empty());
    assert!(!report.warnings.is_empty());
    // Advisory only: the promotion still landed in the codex.
    assert!(g.codex().unwrap().exists(report.promoted[0]).unwrap());
}

#[test]
fn two_resolvers_cannot_both_close_one_candidate() {
    let (d, g) = gng();
    write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    let cid = out.conflicts[0].id;

    // A second handle over the same root: the queue flock serializes the
    // Open recheck, so only the first resolver closes the record.
    let g2 = Gungnir::open(d.path()).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let b2 = barrier.clone();
    let t = std::thread::spawn(move || {
        b2.wait();
        g2.resolve_conflict(cid, ConflictVerb::Dismiss, "other")
    });
    barrier.wait();
    let mine = g.resolve_conflict(cid, ConflictVerb::Contradict, "ops");
    let theirs = t.join().unwrap();

    let (oks, errs): (Vec<_>, Vec<_>) = [mine, theirs].into_iter().partition(|r| r.is_ok());
    assert_eq!(oks.len(), 1);
    assert_eq!(errs.len(), 1);
    let err = errs[0].as_ref().unwrap_err().to_string();
    assert!(err.contains("already"), "{err}");
    assert_eq!(g.all_conflicts().unwrap().len(), 1);
}

#[test]
fn re_resolving_a_crash_applied_contradiction_closes_the_candidate() {
    let (_d, g) = gng();
    let old = write_verified(&g, "sessions use mysql");
    let out = add_entry(&g, "sessions use postgres");
    let cid = out.conflicts[0].id;

    // Crash mid-resolve: the entry mutation applied, the record never
    // closed — the candidate must stay open and re-resolvable.
    g.contradict(old.id, out.id, "ops", None).unwrap();
    assert_eq!(g.open_conflicts().unwrap().len(), 1);

    let rec = g
        .resolve_conflict(cid, ConflictVerb::Contradict, "ops")
        .unwrap();
    assert_eq!(rec.status, ConflictStatus::ResolvedContradicted);
    assert!(g.open_conflicts().unwrap().is_empty());
}

#[test]
fn reviews_of_different_targets_do_not_alias_on_dedupe() {
    let (_d, g) = gng();
    let a = write_verified(&g, "sessions use mysql");
    let b = write_verified(&g, "dns via cloudflare");
    let s = g.start_session("builder", "review round").unwrap();

    // Identical prose, different targets: two promotions, not one alias.
    let promo = |target: EntryId| gungnir::Promotion {
        kind: EntryKind::Review,
        summary: "approved".into(),
        body: "checks passed".into(),
        review_of: Some(target),
    };
    let report = g
        .end_session(&s, "done", vec![promo(a.id), promo(b.id)])
        .unwrap();
    assert_eq!(report.promoted.len(), 2);
    assert_ne!(report.promoted[0], report.promoted[1]);

    let codex = g.codex().unwrap();
    assert_eq!(
        codex.require(report.promoted[0]).unwrap().review_of,
        Some(a.id)
    );
    assert_eq!(
        codex.require(report.promoted[1]).unwrap().review_of,
        Some(b.id)
    );
}

#[test]
fn retried_end_scans_the_promotions_it_finishes() {
    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");
    let s = g.start_session("builder", "migrate sessions").unwrap();

    // Crash mid-end: the journal archive landed, scratch still exists,
    // and the promotion never did. The next end converges — and must
    // still scan what it lands.
    let mut archive = Entry::new("builder", EntryKind::Session, "migrated sessions");
    archive.session_id = Some(s.id.clone());
    g.journal("builder").unwrap().create(&archive).unwrap();

    let report = g
        .end_session(
            &s,
            "migrated sessions",
            vec![gungnir::Promotion {
                kind: EntryKind::Decision,
                summary: "sessions use postgres".into(),
                body: "benchmarked".into(),
                review_of: None,
            }],
        )
        .unwrap();
    assert!(report.already_ended);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(g.open_conflicts().unwrap().len(), 1);
}

#[test]
fn retried_end_rescans_promotions_the_crashed_end_landed() {
    let (_d, g) = gng();
    write_verified(&g, "sessions use mysql");
    let s = g.start_session("builder", "migrate sessions").unwrap();

    // Crash after the archive and the promotion landed but before the
    // conflict scan ran — the entry dedupes out of "fresh" on retry, so
    // only a rescan of all session promotions recovers its candidate.
    let mut archive = Entry::new("builder", EntryKind::Session, "migrated sessions");
    archive.session_id = Some(s.id.clone());
    g.journal("builder").unwrap().create(&archive).unwrap();
    let codex = g.codex().unwrap();
    let mut landed = Entry::new("builder", EntryKind::Decision, "sessions use postgres");
    landed.session_id = Some(s.id.clone());
    landed.body = "benchmarked".into();
    codex.create(&landed).unwrap();

    let report = g
        .end_session(
            &s,
            "migrated sessions",
            vec![gungnir::Promotion {
                kind: EntryKind::Decision,
                summary: "sessions use postgres".into(),
                body: "benchmarked".into(),
                review_of: None,
            }],
        )
        .unwrap();
    assert!(report.already_ended);
    // Deduped to the landed entry — and still scanned for conflicts.
    assert_eq!(report.promoted, vec![landed.id]);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(g.open_conflicts().unwrap().len(), 1);

    // Idempotent: a second retry rescans the same pair but must not
    // queue a second record.
    let again = g
        .end_session(
            &s,
            "migrated sessions",
            vec![gungnir::Promotion {
                kind: EntryKind::Decision,
                summary: "sessions use postgres".into(),
                body: "benchmarked".into(),
                review_of: None,
            }],
        )
        .unwrap();
    assert_eq!(again.conflicts.len(), 1);
    assert_eq!(g.open_conflicts().unwrap().len(), 1);
    assert_eq!(g.all_conflicts().unwrap().len(), 1);

    // Settled stays settled: dismissing the candidate and retrying again
    // must not reopen the pair — detection still fires on rescan, so the
    // queue dedupes on every record for the pair, not only open ones.
    let cid = report.conflicts[0].id;
    g.resolve_conflict(cid, ConflictVerb::Dismiss, "ops")
        .unwrap();
    let third = g
        .end_session(
            &s,
            "migrated sessions",
            vec![gungnir::Promotion {
                kind: EntryKind::Decision,
                summary: "sessions use postgres".into(),
                body: "benchmarked".into(),
                review_of: None,
            }],
        )
        .unwrap();
    assert!(third.conflicts.is_empty());
    assert!(g.open_conflicts().unwrap().is_empty());
    assert_eq!(g.all_conflicts().unwrap().len(), 1);
}

// AEV-44: whole 200-char source-unit summaries describe distinct episodes.
// Two of them share topical stemmed tokens and carry many private tokens,
// but they are not successive values of one fact. `shared_frame` used to
// flag that topical overlap and bounded `resolve --supersede` joined the
// episodes with false `revises` chains (GUNG-21, GUNG-22).
const EPISODE_A: &str = "In Tuesday's planning call we walked through the checkout migration and Priya said the team would keep the mysql sessions for now while the cache rollout stayed on hold until the next sprint review and the deploy window was left as planned.";
const EPISODE_B: &str = "Wednesday's retrospective covered the onboarding funnel instead, Marcus noted that mysql sessions stayed stable through the load test and the search index rebuild was deferred to the next sprint review after the staging outage, while Priya asked for a fresh cost estimate from the finance team before the deploy window changed again.";

/// Persisted summaries are capped at 200 chars by `validate_entry`, the same
/// way AEV-25's promotion-style ingest capped each source-unit summary.
fn capped(s: &str) -> String {
    s.chars().take(200).collect()
}

fn write_verified_long(g: &Gungnir, summary: &str) -> Entry {
    write_verified(g, &capped(summary))
}

/// `detect_many` does not validate length, so a >200-char claim can be
/// scanned against the canonical store directly. AEV-25 called it exactly
/// this way over its promotion batch, with one whole-source-unit summary per
/// entry. Caps the stored `old` claim; returns the raw candidate count.
fn detect_verified_against(old_summary: &str, new_summary: &str) -> usize {
    let (_d, g) = gng();
    write_verified_long(&g, old_summary);
    let codex = g.codex().unwrap();
    let new = Entry::new("a", EntryKind::Decision, new_summary);
    gungnir::conflicts::detect_many(&codex, "codex", std::slice::from_ref(&new), None)
        .unwrap()
        .len()
}

#[test]
fn distinct_episodes_with_shared_topical_tokens_do_not_become_candidates() {
    // >200 chars, sharing many topical tokens (mysql, sessions, checkout,
    // sprint, review, deploy, window) but describing a different episode.
    assert!(EPISODE_B.chars().count() > 200);
    assert_eq!(detect_verified_against(EPISODE_A, EPISODE_B), 0);

    // The persisted 200-char shape must not flag either.
    let (_d, g) = gng();
    write_verified_long(&g, EPISODE_A);
    let out = add_entry(&g, &capped(EPISODE_B));
    assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
    assert!(g.open_conflicts().unwrap().is_empty());
    // Advisory by birth: the write landed, nothing was resolved or linked.
    assert!(g.codex().unwrap().exists(out.id).unwrap());
    assert!(g
        .codex()
        .unwrap()
        .require(out.id)
        .unwrap()
        .revises
        .is_none());
}

#[test]
fn unrelated_long_claims_on_one_topic_do_not_become_candidates() {
    let old = "During the architecture sync the group reviewed the checkout service and agreed the mysql session store needed more capacity before the holiday traffic while the alerting dashboard stayed unchanged for another week.";
    let new = "The Friday incident review looked at the same checkout service again and Priya confirmed mysql sessions held up under the spike but the retry policy for the payment worker was rewritten after the queue backed up and the alerting dashboard gained a new panel.";
    assert!(new.chars().count() > 200);
    assert_eq!(detect_verified_against(old, new), 0);
    let (_d, g) = gng();
    write_verified_long(&g, old);
    let out = add_entry(&g, &capped(new));
    assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
}

#[test]
fn numeric_change_on_a_long_claim_still_becomes_a_candidate() {
    let (_d, g) = gng();
    let old = "The checkout service timeout is 30s and the mysql session pool stays healthy through the sprint review, with the alerting dashboard unchanged for another week.";
    let new = "The checkout service timeout is 60s and the mysql session pool stays healthy through the sprint review, with the alerting dashboard unchanged for another week.";
    write_verified(&g, old);
    let out = add_entry(&g, new);
    assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
    assert!(out.conflicts[0]
        .signals
        .contains(&gungnir::ConflictSignal::NumericMismatch));
    // `shared_frame` is context only; it must not be the reason a long
    // summary qualifies. The explicit numeric flip is the actionable signal.
    assert!(!out.conflicts[0]
        .signals
        .contains(&gungnir::ConflictSignal::SharedFrame));
}

#[test]
fn negation_change_on_a_long_claim_still_becomes_a_candidate() {
    let (_d, g) = gng();
    let old = "The checkout team will deploy on fridays after the sprint review, with mysql sessions staying healthy and the alerting dashboard unchanged for another week.";
    let new = "The checkout team will never deploy on fridays after the sprint review, with mysql sessions staying healthy and the alerting dashboard unchanged for another week.";
    write_verified(&g, old);
    let out = add_entry(&g, new);
    assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
    assert!(out.conflicts[0]
        .signals
        .contains(&gungnir::ConflictSignal::NegationAsymmetry));
    assert!(!out.conflicts[0]
        .signals
        .contains(&gungnir::ConflictSignal::SharedFrame));
}

#[test]
fn focused_value_swap_still_becomes_a_candidate() {
    // A focused claim (a handful of stemmed tokens) that changes one value
    // is the update chain the conflict loop exists to surface.
    let (_d, g) = gng();
    write_verified(&g, "checkout team lead is Priya");
    let out = add_entry(&g, "checkout lead now Marcus");
    assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
    assert!(out.conflicts[0]
        .signals
        .contains(&gungnir::ConflictSignal::SharedFrame));
}

#[test]
fn focused_gate_boundary_is_exactly_twelve_unique_stems() {
    // The gate counts unique stemmed tokens through `recall::tokenize`
    // (lowercase, stopwords stripped, stemmed), not characters. Pin the
    // inclusive cutoff so an off-by-one (`<` instead of `<=`) cannot pass:
    // a 12-stem focused value swap raises SharedFrame, and the same pair
    // plus one extra stem (13) raises no candidate at all.
    use gungnir::recall::tokenize;

    let stems = |s: &str| {
        tokenize(s)
            .into_iter()
            .collect::<std::collections::HashSet<String>>()
            .len()
    };
    let frame = "checkout team lead sprint review deploy window policy audit roster quorum";
    let old_12 = format!("{frame} priya");
    let new_12 = format!("{frame} marcus");
    let old_13 = format!("{frame} handoff priya");
    let new_13 = format!("{frame} handoff marcus");
    assert_eq!(stems(&old_12), 12);
    assert_eq!(stems(&new_12), 12);
    assert_eq!(stems(&old_13), 13);
    assert_eq!(stems(&new_13), 13);

    let (_d, g) = gng();
    write_verified(&g, &old_12);
    let out = add_entry(&g, &new_12);
    assert_eq!(out.conflicts.len(), 1, "{:?}", out.conflicts);
    assert!(out.conflicts[0]
        .signals
        .contains(&gungnir::ConflictSignal::SharedFrame));

    // One extra shared stem makes both sides 13: no lexical signal applies
    // (no digits, no negation), so the pair must not be a candidate.
    let (_d2, g2) = gng();
    write_verified(&g2, &old_13);
    let out = add_entry(&g2, &new_13);
    assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
}

#[test]
fn unverified_old_fact_never_flags_even_with_shared_topical_tokens() {
    let (_d, g) = gng();
    let old = Entry::new("a", EntryKind::Decision, capped(EPISODE_A));
    assert_eq!(old.verification, VerificationState::Unverified);
    g.add(old, Layer::Codex).unwrap();
    let out = add_entry(&g, &capped(EPISODE_B));
    assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
    assert!(g.open_conflicts().unwrap().is_empty());
}

#[test]
fn a_one_sided_addition_never_becomes_a_candidate() {
    // Restating a long claim with one extra acknowledgment word is not a
    // value swap: only one side carries a private token. This invariant held
    // before AEV-44 and must survive the narrowing, which keeps the
    // two-sided private-token requirement rather than widening it.
    let (_d, g) = gng();
    let base = "the checkout service kept mysql sessions healthy through the sprint review with the alerting dashboard unchanged for another week";
    let old = format!("noted {base}");
    write_verified(&g, &old);
    let out = add_entry(&g, base);
    assert!(out.conflicts.is_empty(), "{:?}", out.conflicts);
}
