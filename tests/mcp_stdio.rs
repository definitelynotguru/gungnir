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
        (4u64, serde_json::json!({
            "name": "add_observation",
            "arguments": {"session_id": sid, "agent": "builder",
                          "text": "EXPLAIN shows seq scan on orders"}
        })),
        (5, serde_json::json!({
            "name": "add_attempt",
            "arguments": {"session_id": sid, "agent": "builder",
                          "text": "rewrote query with index hint", "succeeded": true}
        })),
        (6, serde_json::json!({
            "name": "end_session",
            "arguments": {"session_id": sid, "agent": "builder",
                          "summary": "rewrote checkout query"}
        })),
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
    assert!(recall_text.contains("rewrote checkout query"), "{recall_text}");

    // Scratch cleared after end: observing again still works (fresh scratch).
    let again = p.rpc(&msg(
        8,
        "tools/call",
        serde_json::json!({
            "name": "add_observation",
            "arguments": {"session_id": sid, "agent": "builder", "text": "post-session note"}
        }),
    ));
    assert_eq!(again["result"]["isError"], false, "{again}");
}
