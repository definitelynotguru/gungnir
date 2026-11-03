//! CLI end to end for the conflict loop: `add` raises a candidate against a
//! verified fact, `conflicts` lists it, `conflicts resolve --supersede`
//! closes it by linking the revision chain, and `contradict` reaches the
//! state that was previously API-only.

use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, args: &[&str]) -> Output {
    let out = Command::new(env!("CARGO_BIN_EXE_gungnir"))
        .args(args)
        .env("GUNGNIR_ROOT", root)
        .output()
        .expect("spawn gungnir");
    assert!(
        out.status.success(),
        "gungnir {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn conflict_loop_over_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // A verified fact in the codex.
    let old_id = stdout(&run(
        root,
        &[
            "add",
            "--agent",
            "a",
            "--kind",
            "decision",
            "--summary",
            "sessions use mysql",
        ],
    ))
    .trim()
    .to_string();
    run(root, &["verify", &old_id]);

    // A disagreeing write lands and raises a candidate on stderr.
    let out = run(
        root,
        &[
            "add",
            "--agent",
            "a",
            "--kind",
            "decision",
            "--summary",
            "sessions use postgres",
        ],
    );
    let new_id = stdout(&out).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("conflict candidate"), "{err}");

    // The queue lists the open pair.
    let list = stdout(&run(root, &["conflicts"]));
    assert!(list.contains("open"), "{list}");
    assert!(list.contains(&new_id), "{list}");
    assert!(list.contains(&old_id), "{list}");
    let candidate_id = list.split_whitespace().next().unwrap().to_string();

    // Resolution by supersession links the chain; the open queue drains.
    let resolved = stdout(&run(
        root,
        &["conflicts", "resolve", &candidate_id, "--supersede"],
    ));
    assert!(resolved.contains("resolved-superseded"), "{resolved}");
    assert!(stdout(&run(root, &["conflicts"])).contains("no open"));
    assert!(stdout(&run(root, &["conflicts", "--all"])).contains("resolved-superseded"));

    // The current view shows only the newer claim.
    let current = stdout(&run(root, &["recall", "sessions use", "--current"]));
    assert!(current.contains(&new_id), "{current}");
    assert!(!current.contains(&old_id), "{current}");

    // `contradict` is reachable from the CLI now.
    let third_id = stdout(&run(
        root,
        &[
            "add",
            "--agent",
            "a",
            "--kind",
            "observation",
            "--summary",
            "unrelated infra note about dns",
        ],
    ))
    .trim()
    .to_string();
    let out = run(root, &["contradict", &third_id, "--by", &new_id]);
    assert!(stdout(&out).contains("contradicted by"));
}
