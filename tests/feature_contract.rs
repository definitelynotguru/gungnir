//! The feature contract in the manifest: `embed` is opt-in and never part
//! of `default`, and `test-embedder` implies `embed`. These hold for every
//! feature set the tests themselves are built with, so this file carries
//! no cfg gates — the day `embed` reaches `default` the behavior tests in
//! `cli_semantic.rs` compile out silently, and this is the check that
//! still fails loudly.

use serde_json::Value;

fn manifest_features() -> Value {
    let out = std::process::Command::new(env!("CARGO"))
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run cargo metadata");
    assert!(out.status.success(), "cargo metadata failed: {out:?}");
    let meta: Value = serde_json::from_slice(&out.stdout).unwrap();
    let package = meta["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "gungnir")
        .expect("gungnir package in metadata");
    package["features"].clone()
}

#[test]
fn embed_is_never_a_default_feature() {
    let features = manifest_features();
    assert!(
        features.get("embed").is_some(),
        "the `embed` feature must exist"
    );
    let default: Vec<&str> = features
        .get("default")
        .and_then(Value::as_array)
        .map(|d| d.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for f in &default {
        assert!(
            *f != "embed" && *f != "test-embedder",
            "`{f}` must stay opt-in: found in `default` {default:?}"
        );
    }
}

#[test]
fn test_embedder_implies_embed() {
    let features = manifest_features();
    let test_embedder = features["test-embedder"]
        .as_array()
        .expect("`test-embedder` feature must exist");
    assert!(
        test_embedder.iter().any(|f| f == "embed"),
        "test-embedder must imply embed: {test_embedder:?}"
    );
}
