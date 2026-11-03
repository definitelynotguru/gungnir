//! The --semantic contract on a default build: error, never silent
//! keyword results.

#![cfg(not(feature = "embed"))]

use std::process::Command;

#[test]
fn recall_semantic_errors_without_the_embed_feature() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "recall",
            "--semantic",
            "postgres",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("embed"), "{stderr}");
}

#[test]
fn brief_semantic_errors_without_the_embed_feature() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args([
            "--root",
            dir.path().to_str().unwrap(),
            "brief",
            "--agent",
            "builder",
            "--semantic",
            "slow checkout",
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("embed"), "{stderr}");
}
