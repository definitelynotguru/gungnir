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
        let child = Command::new(env!("CARGO_BIN_EXE_gungnir"))
            .arg("mcp")
            .env("GUNGNIR_ROOT", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn gungnir mcp");
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
