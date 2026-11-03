//! The audit surface (log / diff / blame / review / entities) and the
//! abstention contract (--require-verified, coverage JSON, exit code 2),
//! exercised through the real CLI binary against a temp store.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{TimeZone, Utc};
use gungnir::{Entry, EntryKind, Gungnir, VerificationState};

fn root() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

fn run(store: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args(["--root", store.to_str().unwrap()])
        .args(args)
        .output()
        .expect("spawn gungnir")
}

fn ok(store: &Path, args: &[&str]) -> String {
    let out = run(store, args);
    assert!(
        out.status.success(),
        "gungnir {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn add(store: &Path, args: &[&str]) -> String {
    let mut full = vec!["add"];
    full.extend_from_slice(args);
    ok(store, &full).trim().to_string()
}

/// A store with: a verified codex entry `e1`, an unverified revision `e2`
/// that supersedes it, and an unverified journal note.
fn seed(store: &Path) -> (String, String, String) {
    let e1 = add(
        store,
        &[
            "--agent",
            "builder",
            "--kind",
            "decision",
            "--summary",
            "checkout uses orders_archive_idx",
            "--project",
            "checkout",
            "--entities",
            "checkout,orders",
        ],
    );
    ok(
        store,
        &[
            "verify",
            &e1,
            "--verifier",
            "team",
            "--note",
            "confirmed in PR 42",
        ],
    );
    let e2 = ok(
        store,
        &[
            "supersede",
            &e1,
            "--agent",
            "builder",
            "--summary",
            "unverified successor candidate",
        ],
    )
    .trim()
    .to_string();
    // journal entry for cross-partition checks
    add(
        store,
        &[
            "--agent",
            "builder",
            "--kind",
            "attempt",
            "--summary",
            "tried vacuum full on orders",
            "--into",
            "journal",
        ],
    );
    (e1, e2, store.display().to_string())
}

#[test]
fn log_id_shows_lineage_and_verification_log() {
    let (_d, store) = root();
    let (e1, e2, _) = seed(&store);

    let out = ok(&store, &["log", &e2]);
    assert!(out.contains(&e2), "{out}");
    assert!(out.contains("<- this entry"), "{out}");
    assert!(out.contains("verification log"), "{out}");
    // e2's own history: created only.
    assert!(out.contains("(none)"), "{out}");

    let out = ok(&store, &["log", &e1]);
    assert!(out.contains("verified by team"), "{out}");
    assert!(out.contains("confirmed in PR 42"), "{out}");
    // e2 supersedes e1, so it shows below e1 in the lineage.
    assert!(out.contains(&e2), "{out}");
}

#[test]
fn log_scan_is_chronological_and_filterable() {
    let (_d, store) = root();
    let (e1, _, _) = seed(&store);

    let out = ok(&store, &["log"]);
    assert!(out.contains("created"), "{out}");
    assert!(out.contains("verified"), "{out}");
    assert!(out.contains(&e1), "{out}");
    assert!(out.contains("journal/builder"), "{out}");

    // --agent matches the actor: verification events by `team` survive,
    // creations (actor = builder) do not.
    let out = ok(&store, &["log", "--agent", "team"]);
    assert!(out.contains("verified"), "{out}");
    assert!(!out.contains("created"), "{out}");

    // --layer restricts partitions.
    let out = ok(&store, &["log", "--layer", "codex"]);
    assert!(!out.contains("journal/"), "{out}");

    // --since past all events prints the empty verdict.
    let out = ok(&store, &["log", "--since", "2999-01-01T00:00:00Z"]);
    assert!(out.contains("(no events)"), "{out}");

    // Unknown layer is an error, not a silent codex read.
    let out = run(&store, &["log", "--layer", "bogus"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown layer"));
}

#[test]
fn log_id_rejects_scan_filters() {
    let (_d, store) = root();
    let (e1, _, _) = seed(&store);
    let out = run(&store, &["log", &e1, "--agent", "builder"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn diff_reports_added_verified_superseded_contradicted_rolled_back() {
    let (_d, store) = root();
    let g = Gungnir::open(&store).unwrap();
    let codex = g.codex().unwrap();
    let t1 = "2026-01-01T00:00:00Z";
    let t2 = "2027-06-01T00:00:00Z";
    let old = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

    // Present before t1, verified between t1 and t2.
    let mut v = Entry::new("a", EntryKind::Decision, "postgres pooling verified later");
    v.timestamp = old;
    codex.create(&v).unwrap();
    g.verify(v.id, "team", None).unwrap();

    // Present before t1, superseded between.
    let mut s = Entry::new("a", EntryKind::Decision, "mysql sessions superseded later");
    s.timestamp = old;
    codex.create(&s).unwrap();
    let s2 = g
        .supersede(s.id, "a", "postgres sessions head", "")
        .unwrap()
        .id;

    // Present before t1, contradicted and rolled back between.
    let counter = Entry::new("a", EntryKind::Observation, "counter evidence entry");
    codex.create(&counter).unwrap();
    let mut c = Entry::new(
        "a",
        EntryKind::Observation,
        "flapping balancer contradicted later",
    );
    c.timestamp = old;
    codex.create(&c).unwrap();
    g.contradict(c.id, counter.id, "ops", None).unwrap();
    let mut r = Entry::new("a", EntryKind::Decision, "redis cache rolled back later");
    r.timestamp = old;
    codex.create(&r).unwrap();
    {
        let mut r2 = codex.require(r.id).unwrap();
        r2.mark_rolled_back("ops");
        codex.update(&r2).unwrap();
    }

    let out = ok(&store, &["diff", t1, t2]);
    assert!(out.contains("added ("), "{out}");
    assert!(out.contains("verified (1)"), "{out}");
    assert!(out.contains(&format!("{}", v.id)), "{out}");
    assert!(out.contains("superseded (1)"), "{out}");
    assert!(out.contains(&format!("{}", s.id)), "{out}");
    assert!(out.contains(&format!("by {s2}")), "{out}");
    assert!(out.contains("contradicted (1)"), "{out}");
    assert!(out.contains(&format!("{}", c.id)), "{out}");
    assert!(out.contains("rolled back (1)"), "{out}");
    assert!(out.contains(&format!("{}", r.id)), "{out}");

    // A topic filter restricts both snapshots: "sessions" matches the
    // superseded pair but not the verified pooling fact.
    let out = ok(&store, &["diff", t1, t2, "--topic", "sessions"]);
    assert!(out.contains(&format!("{}", s.id)), "{out}");
    assert!(!out.contains(&format!("{}", v.id)), "{out}");

    // t2 < t1 is rejected.
    let out = run(&store, &["diff", t2, t1]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn blame_resolves_ids_and_queries_into_lineage() {
    let (_d, store) = root();
    let (e1, e2, _) = seed(&store);

    let out = ok(&store, &["blame", &e1]);
    assert!(out.contains("belief state: verified"), "{out}");
    assert!(out.contains("verified by team"), "{out}");
    assert!(out.contains(&format!("replaced by: {e2}")), "{out}");

    let out = ok(&store, &["blame", &e2]);
    assert!(out.contains(&format!("supersedes: {e1}")), "{out}");
    assert!(out.contains("replaced by: nothing"), "{out}");

    // A query resolves to the best belief.
    let out = ok(&store, &["blame", "orders_archive_idx"]);
    assert!(out.contains(&e1), "{out}");

    // Nothing matching is a clean error.
    let out = run(&store, &["blame", "zzz nothing like this exists"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no belief matches"));
}

#[test]
fn review_groups_unverified_with_verbs() {
    let (_d, store) = root();
    let (e1, e2, _) = seed(&store);
    let g = Gungnir::open(&store).unwrap();

    // A machine-extracted fact joins the queue flagged.
    let mut x = Entry::new("builder", EntryKind::Observation, "extracted prefers vim");
    x.source_tool = Some("extract".into());
    g.codex().unwrap().create(&x).unwrap();

    let out = ok(&store, &["review"]);
    assert!(out.contains("codex"), "{out}");
    assert!(out.contains("journal/builder"), "{out}");
    assert!(out.contains(&e2), "{out}");
    assert!(out.contains(&x.id.to_string()), "{out}");
    assert!(out.contains("[extracted]"), "{out}");
    assert!(out.contains(&format!("gungnir verify {e2}")), "{out}");
    assert!(out.contains(&format!("gungnir contradict {e2}")), "{out}");
    assert!(out.contains(&format!("gungnir supersede {e2}")), "{out}");
    // Verified and superseded-tail beliefs are not review candidates.
    assert!(!out.contains(&format!("    {e1}")), "{out}");

    let out = ok(&store, &["review", "--layer", "codex"]);
    assert!(!out.contains("journal/builder"), "{out}");
}

#[test]
fn entities_index_and_wikilink_validation() {
    let (_d, store) = root();
    let (e1, _e2, _) = seed(&store);

    // Frontmatter entities index: e1 has checkout+orders, and e2 inherits
    // both through supersede — a revision stays about the same topic.
    let out = ok(&store, &["entities"]);
    assert!(out.contains("checkout\t2"), "{out}");
    assert!(out.contains("orders\t2"), "{out}");

    let out = ok(&store, &["entities", "checkout"]);
    assert!(out.contains(&e1), "{out}");

    // A [[name]] body mention registers as an entity.
    let m = add(
        &store,
        &[
            "--agent",
            "builder",
            "--kind",
            "observation",
            "--summary",
            "grafana dashboard note",
            "--body",
            "see the [[grafana]] board",
        ],
    );
    let out = ok(&store, &["entities", "grafana"]);
    assert!(out.contains(&m), "{out}");

    // An [[id]] wikilink must resolve, like an evidence ref.
    let ok_link = run(
        &store,
        &[
            "add",
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "linked note",
            "--body",
            &format!("builds on [[{e1}]]"),
        ],
    );
    assert!(
        ok_link.status.success(),
        "{}",
        String::from_utf8_lossy(&ok_link.stderr)
    );

    let dangling = run(
        &store,
        &[
            "add",
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "dangling link",
            "--body",
            "points at [[01ARZ3NDEKTSV4RRFFQ69G5FAV]]",
        ],
    );
    assert_eq!(dangling.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&dangling.stderr).contains("wikilink"));
}

#[test]
fn require_verified_makes_abstention_explicit() {
    let (_d, store) = root();
    let id = add(
        &store,
        &[
            "--agent",
            "a",
            "--kind",
            "decision",
            "--summary",
            "postgres pooling strategy",
        ],
    );

    // Unverified-only coverage: explicit verdict + exit 2.
    let out = run(&store, &["recall", "postgres", "--require-verified"]);
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("no verified knowledge covers this topic (1 matched, 0 verified)"),
        "{stdout}"
    );

    // Machine-readable form: JSON hits + coverage + abstained flag.
    let out = run(
        &store,
        &["recall", "postgres", "--require-verified", "--json"],
    );
    assert_eq!(out.status.code(), Some(2));
    let doc: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(doc["abstained"], true);
    assert_eq!(doc["coverage"]["verified"], 0);
    assert_eq!(doc["coverage"]["unverified"], 1);
    assert_eq!(doc["hits"].as_array().unwrap().len(), 0);

    // Verified coverage flips the verdict: exit 0, hit returned.
    ok(&store, &["verify", &id]);
    let out = run(&store, &["recall", "postgres", "--require-verified"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stdout).contains(&id));

    // A topic with zero matches still reports the abstention verdict.
    let out = run(&store, &["recall", "quantum zeno", "--require-verified"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stdout).contains("(no knowledge matched this topic)"));
}

#[test]
fn brief_require_verified_abstains_with_exit_2() {
    let (_d, store) = root();
    add(
        &store,
        &[
            "--agent",
            "a",
            "--kind",
            "decision",
            "--summary",
            "postgres pooling strategy",
        ],
    );

    let out = run(
        &store,
        &[
            "brief",
            "--agent",
            "a",
            "postgres pooling",
            "--require-verified",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Entries matched this task but none are verified"),
        "{stdout}"
    );
    // The verdict states verified-vs-matching precisely.
    assert!(stdout.contains("0 verified, 1 unverified"), "{stdout}");
}

#[test]
fn contradict_verb_is_wired() {
    let (_d, store) = root();
    let target = add(
        &store,
        &[
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "balancer healthy",
        ],
    );
    let counter = add(
        &store,
        &[
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "balancer flapping",
        ],
    );

    let out = ok(&store, &["contradict", &target, "--by", &counter]);
    assert!(out.contains("contradicted"), "{out}");

    let out = ok(&store, &["log", &target]);
    assert!(out.contains("contradicted"), "{out}");

    // State is visible in the entry itself.
    let g = Gungnir::open(&store).unwrap();
    let e = g.codex().unwrap().require(target.parse().unwrap()).unwrap();
    assert!(matches!(
        e.verification,
        VerificationState::Contradicted { .. }
    ));
}

#[test]
fn blame_lists_body_wikilink_citations_without_evidence() {
    let (_d, store) = root();
    let (e1, _, _) = seed(&store);

    // An entry whose ONLY link to e1 is a body [[id]] — no evidence items.
    let citer = add(
        &store,
        &[
            "--agent",
            "builder",
            "--kind",
            "observation",
            "--summary",
            "follow-up that cites the decision",
            "--body",
            &format!("builds on [[{e1}]]"),
        ],
    );

    let out = ok(&store, &["blame", &e1]);
    assert!(out.contains(&citer), "{out}");
    assert!(out.contains("cited by"), "{out}");
}

#[test]
fn add_accepts_body_wikilink_across_layers() {
    let (_d, store) = root();
    let codex_id = add(
        &store,
        &[
            "--agent",
            "a",
            "--kind",
            "decision",
            "--summary",
            "codex fact to cite",
        ],
    );

    // Journal entry citing a codex id: the union resolver must see it.
    let out = run(
        &store,
        &[
            "add",
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "journal note citing codex",
            "--body",
            &format!("per [[{codex_id}]]"),
            "--into",
            "journal",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn entity_names_enforce_the_cap_on_both_paths() {
    let (_d, store) = root();

    // 40 non-ASCII chars = 80 UTF-8 bytes: the cap is on characters.
    let cjk = "\u{4e2d}".repeat(40);
    let out = run(
        &store,
        &[
            "add",
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "non-ascii entity",
            "--entities",
            &cjk,
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // 65 characters is over the cap from either path.
    let long = "x".repeat(65);
    for args in [
        vec![
            "add",
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "s1",
            "--entities",
            &long,
        ],
        vec![
            "add",
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "s2",
            "--body",
            &format!("see [[{long}]]"),
        ],
    ] {
        let out = run(&store, &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("entity names"));
    }

    // An unterminated [[ is literal text, not a name that swallows lines.
    let unterminated = add(
        &store,
        &[
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "literal bracket note",
            "--body",
            "a [[ half link\nmore text and [[real_entity]] here",
        ],
    );
    let g = Gungnir::open(&store).unwrap();
    let e = g
        .codex()
        .unwrap()
        .require(unterminated.parse().unwrap())
        .unwrap();
    assert_eq!(e.entity_mentions(), vec!["real_entity".to_string()]);
}

#[test]
fn deleted_scratch_link_does_not_brick_updates() {
    let (_d, store) = root();
    let g = Gungnir::open(&store).unwrap();

    // A scratch entry exists only for the session's life.
    let s = g.start_session("builder", "temp task");
    let scratch_id = g.add_observation(&s, "temporary observation").unwrap();

    // promote writes into codex with the union resolver; a body wikilink
    // to the scratch id is valid while the session lives.
    let cid = g
        .promote(
            scratch_id,
            "builder",
            EntryKind::Decision,
            "fact citing scratch work",
            format!("derived from [[{scratch_id}]]"),
        )
        .unwrap();

    // Session ends, scratch is deleted — the link now dangles.
    let _ = g.end_session(&s, "done", vec![]).unwrap();
    assert!(g.locate(scratch_id).unwrap().is_none());

    // Verify must still land: an entry can't be bricked by a link that
    // was valid when written.
    g.verify(cid, "human", None).unwrap();
    let stored = g.codex().unwrap().require(cid).unwrap();
    assert_eq!(stored.verification, VerificationState::Verified);
}
