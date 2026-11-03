//! Conflict-candidate loop end to end: a write beside a live verified fact
//! records a candidate, the queue survives restarts, resolution verbs close
//! it through the existing trust transitions, and briefings warn while the
//! loop is open.

use gungnir::{
    ConflictStatus, ConflictVerb, Entry, EntryKind, Gungnir, Layer, Query, VerificationState,
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
