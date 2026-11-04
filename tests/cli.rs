//! CLI-level tests against the compiled binary: the JSON contracts the
//! process prints are part of its public surface, so they are exercised
//! end to end rather than reconstructed in-process.

use std::process::Command;

use gungnir::Gungnir;

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
    let scout = g.start_session("scout", "fix checkout latency");
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
