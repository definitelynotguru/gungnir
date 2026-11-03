//! P0 integrity regressions: dead schema wiring, read-path resilience,
//! honest abstention, session lifecycle leaks.
//!
//! Each test asserts behavior a caller can observe, named for the bug it
//! pins. Library-level tests drive the facade; CLI-level tests spawn the
//! real binary through `CARGO_BIN_EXE_gungnir`.

use std::process::Command;

use gungnir::{Entry, EntryKind, Error, Evidence, Gungnir, Query, Session, VerificationState};

fn gng() -> (tempfile::TempDir, Gungnir) {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    (dir, g)
}

fn cli(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args(args)
        .env("GUNGNIR_ROOT", root)
        .output()
        .expect("spawn gungnir cli")
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn add_codex(root: &std::path::Path, summary: &str) -> String {
    let out = cli(
        root,
        &[
            "add",
            "--agent",
            "a",
            "--kind",
            "decision",
            "--summary",
            summary,
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    stdout(&out).trim().to_string()
}

// -- dead schema ------------------------------------------------------------

#[test]
fn cli_contradict_reaches_the_contradicted_state() {
    let (dir, g) = gng();
    let codex = g.codex().unwrap();
    let counter = Entry::new("a", EntryKind::Observation, "dashboard shows flapping");
    codex.create(&counter).unwrap();
    let claim = Entry::new("a", EntryKind::Observation, "load balancer healthy");
    codex.create(&claim).unwrap();

    let cid = claim.id.to_string();
    let by = counter.id.to_string();
    let out = cli(
        dir.path(),
        &[
            "contradict",
            &cid,
            "--by",
            &by,
            "--note",
            "dashboard disagrees",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("contradicted"));

    let after = codex.require(claim.id).unwrap();
    assert_eq!(
        after.verification,
        VerificationState::Contradicted { by: counter.id }
    );
    // The log carries the reason.
    let last = after.verification_log.last().unwrap();
    assert_eq!(last.note.as_deref(), Some("dashboard disagrees"));

    // Contradicted entries show up in coverage and briefing warnings.
    let out = g
        .search_layer(
            gungnir::Layer::Codex,
            &Query::new("load balancer", 10).current(),
        )
        .unwrap();
    assert_eq!(out.coverage.contradicted, 1);
    let b = g.brief("a", "load balancer", 8).unwrap();
    assert!(b.markdown.contains("contradicted"), "{}", b.markdown);
}

#[test]
fn mcp_contradict_tool_reaches_the_contradicted_state() {
    let (dir, g) = gng();
    let codex = g.codex().unwrap();
    let counter = Entry::new("a", EntryKind::Observation, "counter evidence");
    codex.create(&counter).unwrap();
    let claim = Entry::new("a", EntryKind::Observation, "the claim");
    codex.create(&claim).unwrap();

    let mut server = gungnir::mcp::Server::new(Gungnir::open(dir.path()).unwrap());
    let input = format!(
        concat!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"contradict","arguments":{{"id":"{id}","by":"{by}"}}}}}}"#,
            "\n"
        ),
        id = claim.id,
        by = counter.id
    );
    let mut out: Vec<u8> = Vec::new();
    server
        .serve(std::io::BufReader::new(input.as_bytes()), &mut out)
        .unwrap();
    let msg: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(msg["result"]["isError"], false, "{msg}");

    assert_eq!(
        g.codex().unwrap().require(claim.id).unwrap().verification,
        VerificationState::Contradicted { by: counter.id }
    );
}

#[test]
fn cli_add_kind_review_sets_review_of() {
    let (dir, g) = gng();
    let target = add_codex(dir.path(), "use postgres for sessions");

    let out = cli(
        dir.path(),
        &[
            "add",
            "--agent",
            "reviewer",
            "--kind",
            "review",
            "--summary",
            "re-checked the postgres decision",
            "--review-of",
            &target,
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let rid: gungnir::EntryId = stdout(&out).trim().parse().unwrap();
    let review = g.codex().unwrap().require(rid).unwrap();
    assert_eq!(review.kind, EntryKind::Review);
    assert_eq!(review.review_of, Some(target.parse().unwrap()));

    // Review still fails without the flag.
    let out = cli(
        dir.path(),
        &[
            "add",
            "--agent",
            "r",
            "--kind",
            "review",
            "--summary",
            "no target",
        ],
    );
    assert!(!out.status.success());
}

#[test]
fn supersede_carries_parent_evidence_forward() {
    let (_dir, g) = gng();
    let codex = g.codex().unwrap();
    let s = g.start_session("builder", "investigate").unwrap();
    g.add_observation(&s, "evidence gathered").unwrap();
    let report = g.end_session(&s, "done", vec![]).unwrap();

    let mut parent = Entry::new("a", EntryKind::Decision, "use postgres");
    parent.evidence.push(Evidence::Ref {
        id: report.journal_id,
    });
    codex
        .create_with(&parent, &|id| Ok(g.locate(id)?.is_some()))
        .unwrap();

    let next = g
        .supersede(parent.id, "a", "use cockroach", "scale")
        .unwrap();
    let stored = codex.require(next).unwrap();
    assert_eq!(stored.evidence, parent.evidence);
}

#[test]
fn promote_rejects_non_journal_sources() {
    let (_dir, g) = gng();
    let codex = g.codex().unwrap();

    // Codex source: self-citing.
    let c = Entry::new("a", EntryKind::Decision, "already codex");
    codex.create(&c).unwrap();
    assert!(matches!(
        g.promote(c.id, "a", EntryKind::Decision, "copy", ""),
        Err(Error::Invalid(_))
    ));

    // Scratch source: evidence would dangle when the session ends.
    let s = g.start_session("builder", "work").unwrap();
    let oid = g.add_observation(&s, "live scratch note").unwrap();
    assert!(matches!(
        g.promote(oid, "a", EntryKind::Decision, "copy", ""),
        Err(Error::Invalid(_))
    ));

    // Journal source: the intended path still works.
    let report = g.end_session(&s, "done", vec![]).unwrap();
    let promoted = g
        .promote(
            report.journal_id,
            "a",
            EntryKind::Decision,
            "journal finding",
            "",
        )
        .unwrap();
    let stored = g.codex().unwrap().require(promoted).unwrap();
    assert_eq!(
        stored.evidence,
        vec![Evidence::Ref {
            id: report.journal_id
        }]
    );
}

#[test]
fn typed_log_status_roundtrips_and_typos_fail_loudly() {
    // A misspelled log status used to deserialize fine and silently
    // downgrade the entry to unverified under as_of. Now the file fails to
    // parse — and under skip-and-report reads it is reported, not believed.
    let (_dir, g) = gng();
    let codex = g.codex().unwrap();
    let mut e = Entry::new("a", EntryKind::Decision, "queue backed by redis");
    e.verify("human", None);
    codex.create(&e).unwrap();

    let path = codex.path_for(e.id);
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, raw.replace("status: verified", "status: verrified")).unwrap();

    let scan = codex.scan().unwrap();
    assert!(scan.entries.is_empty());
    assert_eq!(scan.skipped.len(), 1);
    assert!(
        scan.skipped[0].error.contains("frontmatter"),
        "{}",
        scan.skipped[0].error
    );
}

#[test]
fn length_caps_count_chars_not_bytes() {
    let (_dir, g) = gng();
    let codex = g.codex().unwrap();

    // 160 'é' chars = 320 bytes: over the old byte cap, under the spec's
    // 200-char cap.
    let summary = "é".repeat(160);
    let e = Entry::new("a", EntryKind::Observation, summary);
    codex.create(&e).unwrap();

    let over = Entry::new("a", EntryKind::Observation, "é".repeat(201));
    assert!(matches!(codex.create(&over), Err(Error::Invalid(_))));

    let mut ev = Entry::new("a", EntryKind::Observation, "cites a doc");
    ev.evidence.push(Evidence::File {
        path: "docs/x.md".into(),
        excerpt: "é".repeat(450),
        sha256: "00".repeat(32),
    });
    codex.create(&ev).unwrap();
}

// -- read resilience ---------------------------------------------------------

/// A ULID-named file carrying git merge conflict markers: the most likely
/// real-world corruption in a human-edited, git-versioned store.
fn conflicted_file_body() -> String {
    "<<<<<<< HEAD\n---\nid: 01ARZ3NDEKTSV4RRFFQ69G5FAV\n=======\nbody only\n>>>>>>> branch\n"
        .to_string()
}

fn write_conflicted_entry(store: &gungnir::Store) {
    let id: gungnir::EntryId = "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap();
    let path = store.path_for(id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, conflicted_file_body()).unwrap();
}

#[test]
fn corrupt_file_is_skipped_and_reported_across_read_surfaces() {
    let (dir, g) = gng();
    let codex = g.codex().unwrap();
    let good = Entry::new("a", EntryKind::Decision, "sessions use postgres");
    codex.create(&good).unwrap();
    write_conflicted_entry(&codex);

    // Scan reports the skip and keeps the good entries.
    let scan = codex.scan().unwrap();
    assert_eq!(scan.entries.len(), 1);
    assert_eq!(scan.skipped.len(), 1);

    // Recall still works and reports the skip.
    let out = g
        .search_layer(gungnir::Layer::Codex, &Query::new("postgres", 10))
        .unwrap();
    assert_eq!(out.hits.len(), 1);
    assert_eq!(out.skipped.len(), 1);

    // Briefing still works.
    let b = g.brief("a", "postgres", 8).unwrap();
    assert!(b.markdown.contains("postgres"));
    assert_eq!(b.skipped.len(), 1);

    // Stats still work and count the skipped file.
    let st = g.stats(None).unwrap();
    assert_eq!(st.codex_entries, 1);
    assert_eq!(st.skipped.len(), 1);

    // CLI recall exits 0, warns on stderr, and keeps stdout parseable.
    let out = cli(dir.path(), &["recall", "postgres"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("postgres"));
    assert!(stderr(&out).contains("skipped"), "{}", stderr(&out));
}

// -- honest abstention -------------------------------------------------------

#[test]
fn recall_verdict_distinguishes_empty_from_no_match() {
    let (dir, g) = gng();
    let codex = g.codex().unwrap();

    // Empty layer: nothing exists.
    let out = cli(dir.path(), &["recall", "postgres"]);
    assert!(stdout(&out).contains("no knowledge on file"));
    assert!(stdout(&out).contains("coverage:"));

    // Non-empty layer, no token overlap: the verdict names the miss, and the
    // coverage line is no longer suppressed.
    let e = Entry::new("a", EntryKind::Decision, "kubernetes ingress notes");
    codex.create(&e).unwrap();
    let out = cli(dir.path(), &["recall", "quantum entanglement"]);
    assert!(stdout(&out).contains("no knowledge matched this topic"));
    assert!(stdout(&out).contains("coverage:"));
    assert!(!stdout(&out).contains("no knowledge covers this topic"));
}

#[test]
fn coverage_line_survives_hidden_only_matches() {
    let (dir, g) = gng();
    let codex = g.codex().unwrap();

    // One matching entry that is rolled back: previously the coverage line
    // vanished exactly when only hidden matches existed.
    let mut v1 = Entry::new("a", EntryKind::Decision, "sessions use mysql");
    v1.verify("human", None);
    codex.create(&v1).unwrap();
    let v2 = g
        .supersede(v1.id, "a", "sessions use cockroach", "later")
        .unwrap();
    g.rollback(v2, "a").unwrap();

    // mysql only matches the verified ancestor which is visible; query for
    // the rolled-back generation's unique token instead.
    let out = cli(dir.path(), &["recall", "cockroach"]);
    let text = stdout(&out);
    assert!(text.contains("coverage:"), "{text}");
    assert!(
        text.contains("no live knowledge") || text.contains("no knowledge matched"),
        "{text}"
    );
}

// -- session lifecycle -------------------------------------------------------

#[test]
fn session_task_survives_a_rebuilt_handle() {
    let (dir, g) = gng();
    let s = g
        .start_session("builder", "fix slow checkout query")
        .unwrap();
    g.add_attempt(&s, "added index", false).unwrap();

    // Simulate a later process: the handle knows only id + agent.
    let rebuilt = Session {
        id: s.id.clone(),
        agent: "builder".into(),
        task: String::new(),
        started_at: chrono::Utc::now(),
    };
    let report = g.end_session(&rebuilt, "indexed it", vec![]).unwrap();

    let archived = g
        .journal("builder")
        .unwrap()
        .require(report.journal_id)
        .unwrap();
    assert!(
        archived.body.contains("# Task\nfix slow checkout query"),
        "{}",
        archived.body
    );

    // Rebuilding again (fresh root open) still finds the meta before end.
    drop(g);
    let g2 = Gungnir::open(dir.path()).unwrap();
    let s2 = g2.load_session(&s.id).unwrap();
    assert!(s2.is_none(), "scratch is gone after end; meta must be too");
}

#[test]
fn end_session_is_idempotent() {
    let (_dir, g) = gng();
    let s = g.start_session("builder", "fix slow checkout").unwrap();
    g.add_observation(&s, "seq scan").unwrap();

    let first = g.end_session(&s, "done", vec![]).unwrap();
    assert!(!first.already_ended);

    let second = g.end_session(&s, "done", vec![]).unwrap();
    assert!(second.already_ended);
    assert_eq!(second.journal_id, first.journal_id);

    // Only one archive exists for the session.
    let journal = g.journal("builder").unwrap();
    let archives: Vec<_> = journal
        .entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == EntryKind::Session)
        .collect();
    assert_eq!(archives.len(), 1);

    // A session that never existed reports plainly instead of archiving air.
    let ghost = Session {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
        agent: "builder".into(),
        task: String::new(),
        started_at: chrono::Utc::now(),
    };
    assert!(matches!(
        g.end_session(&ghost, "x", vec![]),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn end_session_retry_after_crash_converges() {
    let (_dir, g) = gng();
    let s = g.start_session("builder", "task").unwrap();
    g.add_observation(&s, "note").unwrap();

    // Simulate a crash between the journal write and scratch removal:
    // archive exists, scratch dir still there with entries.
    let report = g.end_session(&s, "done", vec![]).unwrap();
    // Recreate scratch as the crash would have left it. Written directly —
    // add_observation now (correctly) rejects writes to an ended session.
    let mut leftover = Entry::new("builder", EntryKind::Observation, "leftover note");
    leftover.session_id = Some(s.id.clone());
    g.scratch(&s.id).unwrap().create(&leftover).unwrap();

    let retry = g.end_session(&s, "done", vec![]).unwrap();
    assert!(retry.already_ended);
    assert_eq!(retry.journal_id, report.journal_id);
    assert_eq!(g.journal("builder").unwrap().entries().unwrap().len(), 1);
    assert!(!g
        .root()
        .join("scratch")
        .join(gungnir::layout::sanitize_component(&s.id))
        .exists());
}

#[test]
fn reads_do_not_create_directories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("nonexistent-store");
    let g = Gungnir::open(&root).unwrap();

    let st = g.stats(Some("nobody")).unwrap();
    assert_eq!(st.journal_entries, 0);
    assert!(!root.exists(), "stats created the store root");
}

#[test]
fn attempt_outcome_is_structured() {
    let (_dir, g) = gng();
    let s = g.start_session("builder", "task").unwrap();
    let id = g.add_attempt(&s, "tried the index", true).unwrap();
    let scratch = g.scratch(&s.id).unwrap();
    let entry = scratch.require(id).unwrap();
    assert_eq!(entry.outcome, Some(true));

    let report = g.end_session(&s, "done", vec![]).unwrap();
    let archived = g
        .journal("builder")
        .unwrap()
        .require(report.journal_id)
        .unwrap();
    assert!(archived.body.contains("[succeeded]"), "{}", archived.body);
}

#[test]
fn promote_via_mcp_review_kind_links_source() {
    let (_dir, g) = gng();
    let s = g.start_session("builder", "task").unwrap();
    g.add_observation(&s, "note").unwrap();
    let report = g.end_session(&s, "done", vec![]).unwrap();

    let rid = g
        .promote(
            report.journal_id,
            "builder",
            EntryKind::Review,
            "reviewed the session archive",
            "",
        )
        .unwrap();
    let review = g.codex().unwrap().require(rid).unwrap();
    assert_eq!(review.review_of, Some(report.journal_id));
}

// -- review round 1 ----------------------------------------------------------

#[test]
fn end_session_retry_finishes_pending_promotions() {
    let (_dir, g) = gng();
    let s = g.start_session("builder", "task").unwrap();
    g.add_observation(&s, "note").unwrap();

    let first = g
        .end_session(
            &s,
            "done",
            vec![gungnir::Promotion {
                kind: EntryKind::Decision,
                summary: "use the index".into(),
                body: "it fixed the scan".into(),
            }],
        )
        .unwrap();
    assert_eq!(first.promoted.len(), 1);

    // A crash between archive write and scratch removal leaves scratch
    // behind; the caller retries with the full promotion list plus one
    // that never landed. Written directly — add_observation now rejects
    // writes to an ended session.
    let mut leftover = Entry::new("builder", EntryKind::Observation, "leftover note");
    leftover.session_id = Some(s.id.clone());
    g.scratch(&s.id).unwrap().create(&leftover).unwrap();

    let retry = g
        .end_session(
            &s,
            "done",
            vec![
                gungnir::Promotion {
                    kind: EntryKind::Decision,
                    summary: "use the index".into(),
                    body: "it fixed the scan".into(),
                },
                gungnir::Promotion {
                    kind: EntryKind::Observation,
                    summary: "pg_stat confirmed it".into(),
                    body: String::new(),
                },
            ],
        )
        .unwrap();
    assert!(retry.already_ended);
    assert_eq!(retry.journal_id, first.journal_id);

    // The landed promotion is deduped by reference, not duplicated; the
    // pending one is written now.
    assert_eq!(retry.promoted.len(), 2);
    assert_eq!(retry.promoted[0], first.promoted[0]);
    let codex = g.codex().unwrap();
    let session_entries: Vec<_> = codex
        .entries()
        .unwrap()
        .into_iter()
        .filter(|e| e.session_id.as_deref() == Some(s.id.as_str()))
        .collect();
    assert_eq!(session_entries.len(), 2);
    assert!(!g
        .root()
        .join("scratch")
        .join(gungnir::layout::sanitize_component(&s.id))
        .exists());
}

#[test]
fn briefing_verdict_counts_contradicted_matches() {
    let (_dir, g) = gng();
    let codex = g.codex().unwrap();
    let counter = Entry::new("a", EntryKind::Observation, "dashboard shows flapping");
    codex.create(&counter).unwrap();
    let claim = Entry::new("a", EntryKind::Observation, "load balancer healthy");
    codex.create(&claim).unwrap();
    g.contradict(claim.id, counter.id, "ops", None).unwrap();

    let b = g.brief("a", "load balancer", 8).unwrap();
    assert!(
        b.markdown
            .contains("Entries matched this task but none are verified"),
        "{}",
        b.markdown
    );
    assert!(
        !b.markdown.contains("No knowledge matched this task"),
        "{}",
        b.markdown
    );
}

#[test]
fn cli_recall_names_contradicted_only_matches() {
    let (dir, g) = gng();
    let codex = g.codex().unwrap();
    let counter = Entry::new("a", EntryKind::Observation, "counter evidence");
    codex.create(&counter).unwrap();
    let claim = Entry::new("a", EntryKind::Observation, "claim about zorblax");
    codex.create(&claim).unwrap();
    g.contradict(claim.id, counter.id, "ops", None).unwrap();

    // `zorblax` matches only the contradicted entry (the counter's summary
    // shares no tokens), so it is dropped from hits under --current.
    let out = cli(dir.path(), &["recall", "zorblax", "--current"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("only contradicted knowledge matched"),
        "{}",
        stdout(&out)
    );
    assert!(
        !stdout(&out).contains("no knowledge matched this topic"),
        "{}",
        stdout(&out)
    );
}

#[test]
fn supersede_drops_dangling_evidence_refs() {
    let (_dir, g) = gng();
    let codex = g.codex().unwrap();
    let solid = Entry::new("a", EntryKind::Observation, "solid source");
    codex.create(&solid).unwrap();

    // Simulate a store written before promote was journal-only: a codex
    // entry citing a scratch entry that end_session has since deleted.
    let mut stale = Entry::new("a", EntryKind::Decision, "old claim needs correcting");
    stale.evidence.push(Evidence::Ref { id: solid.id });
    stale.evidence.push(Evidence::Ref {
        id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().unwrap(),
    });
    codex.create_with(&stale, &|_id| Ok(true)).unwrap();

    let new_id = g
        .supersede(stale.id, "a", "old claim corrected", "fixed")
        .unwrap();
    let next = codex.require(new_id).unwrap();
    // The resolvable ref carries forward; the dangling one does not block
    // the correction.
    assert_eq!(next.evidence, vec![Evidence::Ref { id: solid.id }]);
}

#[test]
fn mcp_tools_report_skipped_entries() {
    let (dir, g) = gng();
    let codex = g.codex().unwrap();
    let good = Entry::new("a", EntryKind::Decision, "sessions use postgres");
    codex.create(&good).unwrap();
    write_conflicted_entry(&codex);

    let mut server = gungnir::mcp::Server::new(Gungnir::open(dir.path()).unwrap());
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list","arguments":{"layer":"codex"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recall","arguments":{"query":"postgres","layer":"codex"}}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"brief","arguments":{"agent":"a","task":"postgres"}}}"#,
        "\n",
    );
    let mut out: Vec<u8> = Vec::new();
    server
        .serve(std::io::BufReader::new(input.as_bytes()), &mut out)
        .unwrap();
    let text = String::from_utf8(out).unwrap();
    let msgs: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(msgs.len(), 3);
    for msg in &msgs {
        let body = msg["result"]["content"][0]["text"].as_str().unwrap();
        assert!(body.contains("(skipped unreadable entry"), "{msg}");
    }
}

#[test]
fn scan_reports_a_wrongly_sharded_file() {
    let (_dir, g) = gng();
    let codex = g.codex().unwrap();
    let e = Entry::new("a", EntryKind::Decision, "placed by hand");
    codex.create(&e).unwrap();

    // A hand move or git merge can leave a ULID-named file outside its
    // canonical YYYY/MM/DD shard. Reads and mutations agree: the file is
    // reported, never adopted.
    let canonical = codex.path_for(e.id);
    let wrong_dir = codex.root().join("1999/01/01");
    std::fs::create_dir_all(&wrong_dir).unwrap();
    let wrong_path = wrong_dir.join(canonical.file_name().unwrap());
    std::fs::rename(&canonical, &wrong_path).unwrap();

    let scan = codex.scan().unwrap();
    assert!(scan.entries.is_empty());
    assert_eq!(scan.skipped.len(), 1);
    assert!(
        scan.skipped[0].error.contains("wrong shard"),
        "{}",
        scan.skipped[0].error
    );
    assert!(codex.get(e.id).unwrap().is_none());
}

// -- review round 2 ----------------------------------------------------------

#[test]
fn writes_to_an_ended_session_are_rejected() {
    let (_dir, g) = gng();
    let s = g.start_session("builder", "task").unwrap();
    g.add_observation(&s, "note").unwrap();
    let report = g.end_session(&s, "done", vec![]).unwrap();

    // A stale handle's writes must not be accepted: the retry path clears
    // scratch, so anything written now would be deleted unarchived.
    let rebuilt = Session {
        id: s.id.clone(),
        agent: "builder".into(),
        task: "task".into(),
        started_at: s.started_at,
    };
    assert!(matches!(
        g.add_observation(&rebuilt, "post-session note"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        g.add_attempt(&rebuilt, "late attempt", true),
        Err(Error::Invalid(_))
    ));

    // The retry still converges and clears nothing it should keep.
    let retry = g.end_session(&s, "done", vec![]).unwrap();
    assert!(retry.already_ended);
    assert_eq!(retry.journal_id, report.journal_id);
}

#[test]
fn promotions_sharing_a_summary_land_when_kinds_differ() {
    let (_dir, g) = gng();
    let s = g.start_session("builder", "task").unwrap();
    g.add_observation(&s, "note").unwrap();

    let report = g
        .end_session(
            &s,
            "done",
            vec![
                gungnir::Promotion {
                    kind: EntryKind::Decision,
                    summary: "use the index".into(),
                    body: String::new(),
                },
                gungnir::Promotion {
                    kind: EntryKind::Observation,
                    summary: "use the index".into(),
                    body: String::new(),
                },
            ],
        )
        .unwrap();
    assert_eq!(report.promoted.len(), 2);
    assert_ne!(report.promoted[0], report.promoted[1]);
    assert_eq!(g.codex().unwrap().entries().unwrap().len(), 2);
}
