//! The --semantic contract on a test-embedder build: it must reach hybrid
//! search, never silently serve keyword results. `GUNGNIR_TEST_EMBEDDER=1`
//! swaps the shipped fastembed model for the uniform-vector test embedder,
//! so no model download is needed — and that swap only exists in binaries
//! built with the test-only `test-embedder` feature.

#![cfg(all(feature = "embed", feature = "test-embedder"))]

use std::process::Command;

#[test]
fn recall_semantic_reaches_hybrid_search() {
    let dir = tempfile::tempdir().unwrap();
    // Seed via the library; the binary under test only reads.
    let g = gungnir::Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();
    let mut e = gungnir::Entry::new(
        "ops",
        gungnir::EntryKind::Decision,
        "postgres pooling limits",
    );
    e.verify("ops", None);
    codex.create(&e).unwrap();

    // No shared token between query and summary, so a keyword-only answer
    // would find nothing — a hit proves the semantic flag dispatched.
    let out = Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "recall",
            "--semantic",
            "query cache eviction",
        ])
        .env("GUNGNIR_TEST_EMBEDDER", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("postgres pooling limits"), "{stdout}");
}

#[test]
fn extract_semantic_dispatches_past_the_feature_gate() {
    let dir = tempfile::tempdir().unwrap();
    // No backend is extract's own clean no-op; what matters is --semantic
    // no longer errors on this build — the flag reached dispatch, which
    // requires the test embedder where the real model would otherwise
    // init.
    let out = Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "extract",
            "--agent",
            "ops",
            "--text",
            "ran migrations",
            "--semantic",
        ])
        .env("GUNGNIR_TEST_EMBEDDER", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("extraction unavailable"), "{stdout}");
}

#[test]
fn brief_semantic_reaches_hybrid_search() {
    let dir = tempfile::tempdir().unwrap();
    let g = gungnir::Gungnir::open(dir.path()).unwrap();
    let codex = g.codex().unwrap();
    let mut e = gungnir::Entry::new(
        "ops",
        gungnir::EntryKind::Decision,
        "postgres pooling limits",
    );
    e.verify("ops", None);
    codex.create(&e).unwrap();

    // Same proof as recall: no shared token between task and summary, so
    // the briefing can only carry the entry through the vector channel.
    let out = Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "brief",
            "--agent",
            "ops",
            "--semantic",
            "query cache eviction",
        ])
        .env("GUNGNIR_TEST_EMBEDDER", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("postgres pooling limits"), "{stdout}");
}
