//! CLI-level tests against the compiled binary: the JSON contracts the
//! process prints are part of its public surface, so they are exercised
//! end to end rather than reconstructed in-process.

use std::process::Command;

use gungnir::{Entry, EntryKind, Gungnir};

fn gungnir(root: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args(args)
        .env("GUNGNIR_ROOT", root)
        .output()
        .expect("spawn gungnir");
    assert!(
        out.status.success(),
        "gungnir {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn stats_json_exposes_peer_coverage_counts() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let scout = g.start_session("scout", "fix checkout latency").unwrap();
    g.add_attempt(&scout, "added index for checkout latency", false)
        .unwrap();
    g.end_session(&scout, "index did not help", vec![]).unwrap();
    drop(g);

    let stdout = gungnir(
        dir.path(),
        &["stats", "--json", "--topic", "checkout latency"],
    );
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let peers = json
        .get("peer_coverage")
        .unwrap_or_else(|| panic!("stats --json lost peer_coverage: {json}"));
    assert_eq!(peers["agents_attempted"], 1, "{peers}");
    assert_eq!(peers["agents_failed"], 1, "{peers}");
    assert_eq!(peers["entries"], 1, "{peers}");
}

#[test]
fn recall_question_time_resolves_relative_dates_at_the_cli_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let g = Gungnir::open(dir.path()).unwrap();
    let mut entry = Entry::new("eval", EntryKind::Session, "smoker purchase");
    entry.timestamp = chrono::DateTime::parse_from_rfc3339("2026-11-18T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    entry.body = "Bought a smoker on 2026-11-08.".into();
    g.codex().unwrap().create(&entry).unwrap();
    drop(g);

    let stdout = gungnir(
        dir.path(),
        &[
            "recall",
            "Which appliance did I buy 10 days ago?",
            "--question-time",
            "2026-11-18T12:00:00Z",
            "--json",
        ],
    );
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["hits"].as_array().unwrap().len(), 1, "{json}");
    assert_eq!(json["hits"][0]["entry"]["summary"], "smoker purchase");
    assert_eq!(json["coverage"]["unverified"], 1);
}

#[test]
fn recall_rejects_an_invalid_question_time_at_the_cli_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args(["recall", "anything", "--question-time", "not-a-timestamp"])
        .env("GUNGNIR_ROOT", dir.path())
        .output()
        .expect("spawn gungnir");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("bad timestamp 'not-a-timestamp'"),
        "{stderr}"
    );
}
