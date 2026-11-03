//! The --semantic contract on an embed build: it must reach hybrid
//! search, never silently serve keyword results. `GUNGNIR_TEST_EMBEDDER=1`
//! swaps the shipped fastembed model for the uniform-vector test embedder,
//! so no model download is needed.

#![cfg(feature = "embed")]

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
