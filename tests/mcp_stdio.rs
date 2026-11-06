//! Runtime proof for the MCP surface: spawn the real binary, speak
//! newline-delimited JSON-RPC over its stdio, verify the handshake and a
//! full session workflow end to end.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

struct Proc {
    child: Child,
}

impl Proc {
    fn spawn(root: &std::path::Path) -> Self {
        Self::spawn_env(root, &[])
    }

    fn spawn_env(root: &std::path::Path, envs: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_gungnir"));
        cmd.arg("mcp")
            .env("GUNGNIR_ROOT", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped());
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("spawn gungnir mcp");
        Self { child }
    }

    fn rpc(&mut self, line: &str) -> serde_json::Value {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let stdout = self.child.stdout.as_mut().unwrap();
        let mut reader = BufReader::new(stdout);
        let mut response = String::new();
        reader.read_line(&mut response).unwrap();
        serde_json::from_str(response.trim()).expect("valid json-rpc response")
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn msg(id: u64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

#[test]
fn mcp_subprocess_handshake_and_workflow() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = Proc::spawn(dir.path());

    // Handshake.
    let init = p.rpc(&msg(1, "initialize", serde_json::json!({})));
    assert_eq!(init["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(init["result"]["serverInfo"]["name"], "gungnir");

    // Tool listing includes the session lifecycle.
    let tools = p.rpc(&msg(2, "tools/list", serde_json::json!({})));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"start_session"));
    assert!(names.contains(&"end_session"));
    assert!(names.contains(&"contradict"));
    assert!(names.contains(&"conflicts"));
    assert!(names.contains(&"resolve_conflict"));

    // Start a session; the result text carries the briefing and session id.
    let started = p.rpc(&msg(
        3,
        "tools/call",
        serde_json::json!({
            "name": "start_session",
            "arguments": {"agent": "builder", "task": "fix slow checkout"}
        }),
    ));
    assert_eq!(started["result"]["isError"], false);
    let text = started["result"]["content"][0]["text"].as_str().unwrap();
    let sid = text
        .lines()
        .next()
        .unwrap()
        .trim_start_matches("session_id: ")
        .to_string();
    assert!(!sid.is_empty());

    // Record work.
    for (id, call) in [
        (
            4u64,
            serde_json::json!({
                "name": "add_observation",
                "arguments": {"session_id": sid, "agent": "builder",
                              "text": "EXPLAIN shows seq scan on orders"}
            }),
        ),
        (
            5,
            serde_json::json!({
                "name": "add_attempt",
                "arguments": {"session_id": sid, "agent": "builder",
                              "text": "rewrote query with index hint", "succeeded": true}
            }),
        ),
        (
            6,
            serde_json::json!({
                "name": "end_session",
                "arguments": {"session_id": sid, "agent": "builder",
                              "summary": "rewrote checkout query"}
            }),
        ),
    ] {
        let r = p.rpc(&msg(id, "tools/call", call));
        assert_eq!(r["result"]["isError"], false, "{r}");
    }

    // The archived knowledge is retrievable through the same wire.
    let recall = p.rpc(&msg(
        7,
        "tools/call",
        serde_json::json!({
            "name": "recall",
            "arguments": {"query": "checkout rewrite", "layer": "journal", "agent": "builder"}
        }),
    ));
    let recall_text = recall["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        recall_text.contains("rewrote checkout query"),
        "{recall_text}"
    );

    // An archived session is done: observing again is rejected, because a
    // write accepted now would be deleted unarchived on the next end.
    let again = p.rpc(&msg(
        8,
        "tools/call",
        serde_json::json!({
            "name": "add_observation",
            "arguments": {"session_id": sid, "agent": "builder", "text": "post-session note"}
        }),
    ));
    assert_eq!(again["result"]["isError"], true, "{again}");

    // Promotion over the wire: journal finding becomes shared Codex fact.
    let archived = recall_text
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_string();
    let promoted = p.rpc(&msg(
        9,
        "tools/call",
        serde_json::json!({
            "name": "promote",
            "arguments": {"from": archived, "agent": "builder", "kind": "decision",
                          "summary": "checkout rewrites go through orders_archive_idx"}
        }),
    ));
    assert_eq!(promoted["result"]["isError"], false, "{promoted}");
    let codex_id = promoted["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .trim_start_matches("promoted to ")
        .to_string();

    // Verify first, so the later rollback has a verified ancestor to anchor
    // to. Then supersede and roll back; history stays intact throughout.
    let verified = p.rpc(&msg(
        10,
        "tools/call",
        serde_json::json!({
            "name": "verify",
            "arguments": {"id": codex_id, "verifier": "team"}
        }),
    ));
    assert_eq!(verified["result"]["isError"], false, "{verified}");

    // Supersede it, then roll the revision back. History stays intact.
    let superseded = p.rpc(&msg(
        11,
        "tools/call",
        serde_json::json!({
            "name": "supersede",
            "arguments": {"id": codex_id, "agent": "builder",
                          "summary": "covering index preferred over rewrite"}
        }),
    ));
    assert_eq!(superseded["result"]["isError"], false, "{superseded}");
    let revision = superseded["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .trim_start_matches("revision ")
        .split(' ')
        .next()
        .unwrap()
        .to_string();

    let rolled = p.rpc(&msg(
        12,
        "tools/call",
        serde_json::json!({"name": "rollback", "arguments": {"id": revision, "agent": "builder"}}),
    ));
    assert_eq!(rolled["result"]["isError"], false, "{rolled}");

    // list reflects the post-rollback codex.
    let listed = p.rpc(&msg(13, "tools/call", serde_json::json!({"name": "list"})));
    let list_text = listed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(list_text.contains("orders_archive_idx"), "{list_text}");
}

#[test]
fn mcp_conflict_tools_close_the_loop() {
    let dir = tempfile::tempdir().unwrap();

    // Seed through the library: a verified fact, a disagreeing write (which
    // raises a candidate), and an unrelated pair for the bare contradict
    // tool. The wire test then only exercises the new tools.
    let (dns_old, dns_new) = {
        use gungnir::{Entry, EntryKind, Gungnir, Layer};
        let g = Gungnir::open(dir.path()).unwrap();
        let mut old = Entry::new("a", EntryKind::Decision, "sessions use mysql");
        old.verify("review", None);
        g.add(old, Layer::Codex).unwrap();
        g.add(
            Entry::new("scout", EntryKind::Decision, "sessions use postgres"),
            Layer::Codex,
        )
        .unwrap();

        // No topic overlap, so this pair raises no candidate — contradict
        // doesn't require a shared frame.
        let mut dns = Entry::new("a", EntryKind::Decision, "dns via cloudflare");
        dns.verify("review", None);
        let dns_old = dns.id;
        g.add(dns, Layer::Codex).unwrap();
        let dns_new = g
            .add(
                Entry::new(
                    "a",
                    EntryKind::Decision,
                    "agent picked a different resolver library",
                ),
                Layer::Codex,
            )
            .unwrap()
            .id;
        (dns_old.to_string(), dns_new.to_string())
    };

    let mut p = Proc::spawn(dir.path());

    // The bare contradict verb is reachable over the wire.
    let hit = p.rpc(&msg(
        0,
        "tools/call",
        serde_json::json!({
            "name": "contradict",
            "arguments": {"id": dns_old, "by": dns_new, "verifier": "ops"}
        }),
    ));
    assert_eq!(hit["result"]["isError"], false, "{hit}");
    assert!(hit["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("contradicted"));

    // The queue lists the open candidate the disagreeing write raised.
    let list = p.rpc(&msg(
        1,
        "tools/call",
        serde_json::json!({"name": "conflicts", "arguments": {"agent": "ops"}}),
    ));
    let text = list["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("open"), "{text}");
    let cid = text.split_whitespace().next().unwrap().to_string();

    // Resolution by contradiction closes it.
    let resolved = p.rpc(&msg(
        2,
        "tools/call",
        serde_json::json!({
            "name": "resolve_conflict",
            "arguments": {"id": cid, "verb": "contradict", "agent": "ops"}
        }),
    ));
    assert_eq!(resolved["result"]["isError"], false, "{resolved}");

    let list = p.rpc(&msg(
        3,
        "tools/call",
        serde_json::json!({"name": "conflicts", "arguments": {"agent": "ops"}}),
    ));
    let text = list["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("no open"), "{text}");

    let list = p.rpc(&msg(
        4,
        "tools/call",
        serde_json::json!({"name": "conflicts", "arguments": {"agent": "ops", "all": true}}),
    ));
    let text = list["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("resolved-contradicted"), "{text}");
}

#[test]
fn mcp_recall_reports_coverage_and_abstains() {
    let dir = tempfile::tempdir().unwrap();
    let g = gungnir::Gungnir::open(dir.path()).unwrap();
    let mut e = gungnir::Entry::new("a", gungnir::EntryKind::Decision, "postgres pooling");
    e.body = "pool size".into();
    g.codex().unwrap().create(&e).unwrap();
    drop(g);

    let mut p = Proc::spawn(dir.path());
    p.rpc(&msg(1, "initialize", serde_json::json!({})));

    // Machine-readable coverage rides every recall; the abstained flag is
    // the refusal signal a caller can branch on without parsing prose.
    let r = p.rpc(&msg(
        2,
        "tools/call",
        serde_json::json!({
            "name": "recall",
            "arguments": {"query": "postgres", "require_verified": true}
        }),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    let cov_line = text.lines().last().unwrap();
    let cov: serde_json::Value =
        serde_json::from_str(cov_line.trim_start_matches("coverage: ")).unwrap();
    assert_eq!(cov["abstained"], true, "{text}");
    assert_eq!(cov["verified"], 0);
    assert_eq!(cov["unverified"], 1);

    // Verify it, and the same call stops abstaining.
    let v = p.rpc(&msg(
        3,
        "tools/call",
        serde_json::json!({
            "name": "verify",
            "arguments": {"id": e.id.to_string(), "verifier": "team"}
        }),
    ));
    assert_eq!(v["result"]["isError"], false, "{v}");

    let r = p.rpc(&msg(
        4,
        "tools/call",
        serde_json::json!({
            "name": "recall",
            "arguments": {"query": "postgres", "require_verified": true}
        }),
    ));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("postgres pooling"), "{text}");
    let cov: serde_json::Value = serde_json::from_str(
        text.lines()
            .last()
            .unwrap()
            .trim_start_matches("coverage: "),
    )
    .unwrap();
    assert_eq!(cov["abstained"], false);
    assert_eq!(cov["verified"], 1);
}

#[test]
fn mcp_contradict_marks_state() {
    let dir = tempfile::tempdir().unwrap();
    let g = gungnir::Gungnir::open(dir.path()).unwrap();
    let target = gungnir::Entry::new("a", gungnir::EntryKind::Observation, "balancer healthy");
    let counter = gungnir::Entry::new("a", gungnir::EntryKind::Observation, "balancer flapping");
    g.codex().unwrap().create(&target).unwrap();
    g.codex().unwrap().create(&counter).unwrap();
    drop(g);

    let mut p = Proc::spawn(dir.path());
    p.rpc(&msg(1, "initialize", serde_json::json!({})));
    let r = p.rpc(&msg(
        2,
        "tools/call",
        serde_json::json!({
            "name": "contradict",
            "arguments": {"id": target.id.to_string(), "by": counter.id.to_string(), "verifier": "ops"}
        }),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");

    let stored = gungnir::Gungnir::open(dir.path())
        .unwrap()
        .codex()
        .unwrap()
        .require(target.id)
        .unwrap();
    assert!(matches!(
        stored.verification,
        gungnir::VerificationState::Contradicted { .. }
    ));
}

#[test]
fn mcp_stats_reports_counts_and_peer_coverage() {
    let dir = tempfile::tempdir().unwrap();
    {
        use gungnir::{Entry, EntryKind, Gungnir, Layer};
        let g = Gungnir::open(dir.path()).unwrap();
        g.add(
            Entry::new("ops", EntryKind::Decision, "postgres pooling"),
            Layer::Codex,
        )
        .unwrap();
        // A peer journal partition holding one on-topic failed attempt —
        // only its counts may cross, never the text.
        let mut attempt = Entry::new("scout", EntryKind::Attempt, "checkout latency fix");
        attempt.body = "outcome: failed\n\ntried index hint".into();
        g.add(attempt, Layer::Journal { agent: "scout" }).unwrap();
        // An off-topic partition must not inflate the counts.
        g.add(
            Entry::new("ops", EntryKind::Observation, "dns unrelated"),
            Layer::Journal { agent: "watcher" },
        )
        .unwrap();
        // A corrupt file in a third journal: skipped on the stats scan,
        // and the diagnostic must not name its partition's agent.
        let journal = dir.path().join("journal/gremlin");
        std::fs::create_dir_all(&journal).unwrap();
        let fake = Entry::new("gremlin", EntryKind::Observation, "corrupt me");
        std::fs::write(
            journal.join(format!("{}.md", fake.id)),
            "---\nunclosed: [list\n---\n",
        )
        .unwrap();
    }

    let mut p = Proc::spawn(dir.path());
    p.rpc(&msg(1, "initialize", serde_json::json!({})));

    // Both new tools advertise.
    let tools = p.rpc(&msg(2, "tools/list", serde_json::json!({})));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"stats"), "{names:?}");
    assert!(names.contains(&"revalidate"), "{names:?}");

    // Bare stats reports the layer counts.
    let r = p.rpc(&msg(
        3,
        "tools/call",
        serde_json::json!({"name": "stats", "arguments": {}}),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("codex entries      1"), "{text}");
    assert!(text.contains("journal entries    2"), "{text}");
    // The corrupt file reports as skipped with the agent name redacted.
    assert!(text.contains("skipped (unparseable) 1"), "{text}");
    assert!(text.contains("<peer>"), "{text}");
    assert!(!text.contains("gremlin"), "{text}");

    // topic adds aggregate peer coverage: scout's failed attempt counts,
    // watcher's off-topic journal does not, and no peer text crosses.
    let r = p.rpc(&msg(
        4,
        "tools/call",
        serde_json::json!({
            "name": "stats",
            "arguments": {"agent": "builder", "topic": "checkout"}
        }),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("peer coverage      1 agents attempted, 1 failed (1 entries)"),
        "{text}"
    );
    assert!(!text.contains("checkout latency fix"), "{text}");
    assert!(!text.contains("tried index hint"), "{text}");
}

#[test]
fn mcp_revalidate_reports_drift_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    let adr = dir.path().join("adr.md");
    std::fs::write(&adr, "decision: use postgres").unwrap();
    let gone = dir.path().join("gone.md");
    std::fs::write(&gone, "proof").unwrap();
    let entry_id;
    {
        let g = gungnir::Gungnir::open(dir.path()).unwrap();
        let e = gungnir::Entry::new("ops", gungnir::EntryKind::Decision, "use postgres");
        g.codex().unwrap().create(&e).unwrap();
        entry_id = e.id.to_string();
        g.verify_with_evidence(e.id, "team", None, &[adr.clone(), gone.clone()])
            .unwrap();
        std::fs::write(&adr, "decision: use sqlite").unwrap();
        std::fs::remove_file(&gone).unwrap();

        // A corrupt ULID-named file in a peer journal: unreadable on the
        // sweep, and its path must not carry the agent's name back across
        // the wire.
        let journal = dir.path().join("journal/scout");
        std::fs::create_dir_all(&journal).unwrap();
        let fake = gungnir::Entry::new("scout", gungnir::EntryKind::Observation, "corrupt me");
        std::fs::write(
            journal.join(format!("{}.md", fake.id)),
            "---\nunclosed: [list\n---\n",
        )
        .unwrap();
    }

    let mut p = Proc::spawn(dir.path());
    p.rpc(&msg(1, "initialize", serde_json::json!({})));

    // Argument validation mirrors the CLI: neither id nor all, and id
    // plus all together, are rejected.
    for (id, arguments) in [
        (2u64, serde_json::json!({})),
        (3, serde_json::json!({"id": entry_id, "all": true})),
    ] {
        let r = p.rpc(&msg(
            id,
            "tools/call",
            serde_json::json!({"name": "revalidate", "arguments": arguments}),
        ));
        assert_eq!(r["result"]["isError"], true, "{r}");
    }

    // A completed sweep is a normal result even when checks fail — drift
    // is the answer, and the summary line carries the count.
    let r = p.rpc(&msg(
        4,
        "tools/call",
        serde_json::json!({
            "name": "revalidate",
            "arguments": {"id": entry_id}
        }),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("DRIFT"), "{text}");
    assert!(text.contains("MISSING"), "{text}");
    assert!(text.contains("adr.md"), "{text}");
    assert!(
        text.contains("0 intact, 2 drifted, missing, or unreadable"),
        "{text}"
    );

    // all sweeps every layer: the same two failed checks plus the corrupt
    // journal file, reported UNREADABLE with its agent segment redacted.
    let r = p.rpc(&msg(
        5,
        "tools/call",
        serde_json::json!({
            "name": "revalidate",
            "arguments": {"all": true}
        }),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("UNREADABLE"), "{text}");
    assert!(
        text.contains("0 intact, 3 drifted, missing, or unreadable"),
        "{text}"
    );
    assert!(text.contains("<peer>"), "{text}");
    assert!(!text.contains("scout"), "{text}");

    // Fix the drifted file back and restore the missing one: the entry
    // revalidates clean.
    std::fs::write(&adr, "decision: use postgres").unwrap();
    std::fs::write(&gone, "proof").unwrap();
    let r = p.rpc(&msg(
        6,
        "tools/call",
        serde_json::json!({
            "name": "revalidate",
            "arguments": {"id": entry_id}
        }),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("2 intact, 0 drifted, missing, or unreadable"),
        "{text}"
    );
}

/// A hand-edited citation hash is free text; one with a multibyte char
/// crossing byte 8 must render as DRIFT, not panic the server mid-loop.
#[test]
fn mcp_revalidate_renders_weird_hash_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let cited = dir.path().join("file.txt");
    std::fs::write(&cited, "content").unwrap();
    {
        let g = gungnir::Gungnir::open(dir.path()).unwrap();
        let mut e = gungnir::Entry::new("ops", gungnir::EntryKind::Decision, "hand-edited hash");
        e.evidence.push(gungnir::Evidence::File {
            path: cited.display().to_string(),
            excerpt: "content".into(),
            sha256: "abcdefgéh".into(),
        });
        g.codex().unwrap().create(&e).unwrap();
    }

    let mut p = Proc::spawn(dir.path());
    p.rpc(&msg(1, "initialize", serde_json::json!({})));
    let r = p.rpc(&msg(
        2,
        "tools/call",
        serde_json::json!({"name": "revalidate", "arguments": {"all": true}}),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("DRIFT"), "{text}");
    assert!(text.contains("expected abcdefg"), "{text}");

    // The server survived: a follow-up call still answers.
    let r = p.rpc(&msg(
        3,
        "tools/call",
        serde_json::json!({"name": "stats", "arguments": {}}),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
}

/// Redaction must anchor at the store root: a root that itself contains a
/// `journal` component keeps every path outside `<root>/journal/<agent>/`
/// intact, so reported diagnostics still name real files.
#[test]
fn mcp_journal_named_root_keeps_diagnostic_paths_real() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("journal/project");
    let codex_file;
    let journal_file;
    {
        use gungnir::{Entry, EntryKind};
        std::fs::create_dir_all(root.join("codex")).unwrap();
        let fake_c = Entry::new("ops", EntryKind::Observation, "codex corrupt");
        codex_file = root.join("codex").join(format!("{}.md", fake_c.id));
        std::fs::write(&codex_file, "---\nunclosed: [list\n---\n").unwrap();

        std::fs::create_dir_all(root.join("journal/scout")).unwrap();
        let fake_j = Entry::new("scout", EntryKind::Observation, "journal corrupt");
        journal_file = root.join("journal/scout").join(format!("{}.md", fake_j.id));
        std::fs::write(&journal_file, "---\nunclosed: [list\n---\n").unwrap();
    }

    let mut p = Proc::spawn(&root);
    p.rpc(&msg(1, "initialize", serde_json::json!({})));

    // The codex diagnostic reports the file's path intact — the `journal`
    // component inside the root is not touched. The store canonicalizes the
    // root (Windows spells it as a verbatim \\?\ path, and the runner's temp
    // dir may differ from ours), so normalize separators and assert on the
    // root-relative tail `journal/project/<layer>/<file>` — that proves the
    // full store path survived, not just a bare filename.
    let norm = |text: &str| text.replace('\\', "/");
    let codex_tail = format!(
        "journal/project/codex/{}",
        codex_file.file_name().unwrap().to_str().unwrap()
    );
    // The journal file's own diagnostic must redact its partition segment
    // in place: `journal/<peer>/<file>` at the journal-layer position, not
    // a bare `<peer>` that could come from anywhere.
    let journal_tail = format!(
        "journal/project/journal/<peer>/{}",
        journal_file.file_name().unwrap().to_str().unwrap()
    );

    let r = p.rpc(&msg(
        2,
        "tools/call",
        serde_json::json!({"name": "stats", "arguments": {}}),
    ));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("skipped (unparseable) 2"), "{text}");
    let text = norm(text);
    assert!(text.contains(&codex_tail), "{text}");
    assert!(text.contains(&journal_tail), "{text}");
    assert!(!text.contains("scout"), "{text}");

    // Same anchoring in the revalidate sweep.
    let r = p.rpc(&msg(
        3,
        "tools/call",
        serde_json::json!({"name": "revalidate", "arguments": {"all": true}}),
    ));
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    let text = norm(text);
    assert!(text.contains(&codex_tail), "{text}");
    assert!(text.contains(&journal_tail), "{text}");
    assert!(!text.contains("scout"), "{text}");
}

#[test]
fn mcp_brief_require_verified_hides_unverified_hits() {
    let dir = tempfile::tempdir().unwrap();
    let g = gungnir::Gungnir::open(dir.path()).unwrap();
    let e = gungnir::Entry::new(
        "a",
        gungnir::EntryKind::Decision,
        "postgres pooling unverified",
    );
    g.codex().unwrap().create(&e).unwrap();
    drop(g);

    let mut p = Proc::spawn(dir.path());
    p.rpc(&msg(1, "initialize", serde_json::json!({})));

    let r = p.rpc(&msg(
        2,
        "tools/call",
        serde_json::json!({
            "name": "brief",
            "arguments": {"agent": "a", "task": "postgres pooling", "require_verified": true}
        }),
    ));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Entries matched this task but none are verified"),
        "{text}"
    );
    assert!(!text.contains(&e.id.to_string()), "{text}");
}

/// semantic:true over the real binary must error on a default build —
/// keyword results in its place would be a silent degrade.
#[cfg(not(feature = "embed"))]
#[test]
fn mcp_semantic_true_errors_without_the_embed_feature() {
    let dir = tempfile::tempdir().unwrap();
    let mut p = Proc::spawn(dir.path());
    p.rpc(&msg(1, "initialize", serde_json::json!({})));

    for (id, arguments) in [
        (
            2u64,
            serde_json::json!({"query": "postgres", "semantic": true}),
        ),
        (
            3,
            serde_json::json!({"agent": "ops", "task": "cache eviction", "semantic": true}),
        ),
    ] {
        let name = if id == 2 { "recall" } else { "brief" };
        let r = p.rpc(&msg(
            id,
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        ));
        assert_eq!(r["result"]["isError"], true, "{name}: {r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("embed"), "{name}: {text}");
    }
}

/// semantic:true over the real binary must reach hybrid search. The
/// uniform embedder scores every pair cosine 1.0, so the target is
/// reachable through the vector channel alone — no shared keyword token —
/// and a keyword answer can only mean the flag degraded silently.
#[cfg(feature = "test-embedder")]
#[test]
fn mcp_semantic_true_reaches_hybrid_search() {
    let dir = tempfile::tempdir().unwrap();
    let g = gungnir::Gungnir::open(dir.path()).unwrap();
    let mut e = gungnir::Entry::new(
        "ops",
        gungnir::EntryKind::Decision,
        "postgres pooling limits",
    );
    e.verify("ops", None);
    g.codex().unwrap().create(&e).unwrap();
    drop(g);

    let mut p = Proc::spawn_env(dir.path(), &[("GUNGNIR_TEST_EMBEDDER", "1")]);
    p.rpc(&msg(1, "initialize", serde_json::json!({})));

    for (id, arguments) in [
        (
            2u64,
            serde_json::json!({"query": "query cache eviction", "semantic": true}),
        ),
        (
            3,
            serde_json::json!({"agent": "ops", "task": "query cache eviction", "semantic": true}),
        ),
    ] {
        let name = if id == 2 { "recall" } else { "brief" };
        let r = p.rpc(&msg(
            id,
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        ));
        assert_eq!(r["result"]["isError"], false, "{name}: {r}");
        let text = r["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("postgres pooling limits"), "{name}: {text}");
    }
}
