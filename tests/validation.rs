//! Write-time validation rules (SPEC.md §Write-time validation).

use gungnir::{Entry, EntryKind, Error, Evidence, Status, Store, VerificationState};

fn setup() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn open_entries_require_an_assignee() {
    // Structural: Status::Open carries assigned_to by construction, so the
    // rule is unrepresentable-violated at the type level. This test pins the
    // shape: an open entry round-trips its assignee.
    let (_d, store) = setup();
    let mut e = Entry::new("a", EntryKind::Decision, "open question");
    e.status = Status::Open {
        assigned_to: "agent-b".into(),
    };
    store.create(&e).unwrap();
    assert_eq!(
        store.require(e.id).unwrap().status,
        Status::Open {
            assigned_to: "agent-b".into()
        }
    );
}

#[test]
fn review_requires_review_of() {
    let (_d, store) = setup();
    let e = Entry::new("a", EntryKind::Review, "reviewing nothing");
    match store.create(&e) {
        Err(Error::Invalid(msg)) => assert!(msg.contains("review_of"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn review_with_existing_target_is_accepted() {
    let (_d, store) = setup();
    let target = Entry::new("a", EntryKind::Decision, "use sqlite");
    store.create(&target).unwrap();

    let mut review = Entry::new("b", EntryKind::Review, "re-checked decision");
    review.review_of = Some(target.id);
    store.create(&review).unwrap();
}

#[test]
fn revises_must_point_at_existing_entry() {
    let (_d, store) = setup();
    let mut e = Entry::new("a", EntryKind::Decision, "revised decision");
    e.revises = Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap());
    match store.create(&e) {
        Err(Error::Invalid(msg)) => assert!(msg.contains("revises"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn supersession_chain_is_accepted() {
    let (_d, store) = setup();
    let v1 = Entry::new("a", EntryKind::Decision, "use mysql");
    store.create(&v1).unwrap();

    let mut v2 = Entry::new("a", EntryKind::Decision, "use postgres");
    v2.revises = Some(v1.id);
    store.create(&v2).unwrap();
}

#[test]
fn contradicted_requires_existing_counter_entry() {
    let (_d, store) = setup();
    let mut e = Entry::new("a", EntryKind::Observation, "claim");
    e.verification = VerificationState::Contradicted {
        by: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
    };
    match store.create(&e) {
        Err(Error::Invalid(msg)) => assert!(msg.contains("contradicted_by"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn summary_over_200_chars_rejected() {
    let (_d, store) = setup();
    let e = Entry::new("a", EntryKind::Observation, "x".repeat(201));
    assert!(matches!(store.create(&e), Err(Error::Invalid(_))));
}

#[test]
fn evidence_ref_must_exist() {
    let (_d, store) = setup();
    let mut e = Entry::new("a", EntryKind::Observation, "cites ghost");
    e.evidence.push(Evidence::Ref {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
    });
    match store.create(&e) {
        Err(Error::Invalid(msg)) => assert!(msg.contains("evidence"), "{msg}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn evidence_excerpt_over_500_chars_rejected() {
    let (_d, store) = setup();
    let mut e = Entry::new("a", EntryKind::Observation, "long quote");
    e.evidence.push(Evidence::File {
        path: "docs/x.md".into(),
        excerpt: "y".repeat(501),
        sha256: "00".repeat(32),
    });
    assert!(matches!(store.create(&e), Err(Error::Invalid(_))));
}

#[test]
fn update_requires_existing_entry() {
    let (_d, store) = setup();
    let e = Entry::new("a", EntryKind::Observation, "ghost update");
    assert!(matches!(store.update(&e), Err(Error::NotFound(_))));
}
