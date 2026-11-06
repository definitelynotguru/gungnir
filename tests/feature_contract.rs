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

/// Every cargo feature transitively enabled by `roots`, by walking each
/// feature's dependency entries. Only plain names that exist in the
/// feature map count: `dep:name` activates an optional dependency and
/// `dep/feat` names a dependency's feature — neither implies a package
/// feature.
fn expand_features(features: &Value, roots: &[String]) -> std::collections::BTreeSet<String> {
    let mut enabled: std::collections::BTreeSet<String> = roots.iter().cloned().collect();
    let mut queue: Vec<String> = roots.to_vec();
    while let Some(name) = queue.pop() {
        for dep in features[&name].as_array().cloned().unwrap_or_default() {
            let Some(dep) = dep.as_str() else { continue };
            if dep.starts_with("dep:") || dep.contains('/') {
                continue;
            }
            if features.get(dep).is_some() && enabled.insert(dep.to_string()) {
                queue.push(dep.to_string());
            }
        }
    }
    enabled
}

#[test]
fn embed_is_never_a_default_feature() {
    let features = manifest_features();
    assert!(
        features.get("embed").is_some(),
        "the `embed` feature must exist"
    );
    let roots: Vec<String> = features
        .get("default")
        .and_then(Value::as_array)
        .map(|d| {
            d.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    // `default = ["fast"]` plus `fast = ["embed"]` enables the embedder on
    // a plain build, so check what `default` resolves to, not its literals.
    let enabled = expand_features(&features, &roots);
    for f in ["embed", "test-embedder"] {
        assert!(
            !enabled.contains(f),
            "`{f}` must stay opt-in: `default` {roots:?} enables it transitively"
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
