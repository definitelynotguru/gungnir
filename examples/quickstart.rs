//! End-to-end tour of the Gungnir facade: session lifecycle, promotion,
//! briefing, supersession, rollback.
//!
//! Run with: cargo run --example quickstart

use gungnir::{EntryKind, Gungnir, Promotion};

fn main() -> gungnir::Result<()> {
    let root = std::env::temp_dir().join("gungnir-quickstart");
    let _ = std::fs::remove_dir_all(&root);
    let g = Gungnir::open(&root)?;

    // A prior session leaves a failure behind in this agent's journal.
    let past = g.start_session("builder", "speed up checkout");
    g.add_attempt(&past, "added index on orders.user_id", false)?;
    g.end_session(&past, "index did not help", vec![])?;

    // Today's task starts with a briefing assembled from codex + journal.
    let s = g.start_session("builder", "fix slow checkout query");
    g.add_observation(&s, "EXPLAIN shows seq scan on orders_archive")?;
    g.add_attempt(&s, "rewrote query to use orders_archive_idx", true)?;

    let report = g.end_session(
        &s,
        "rewrote checkout query to use orders_archive_idx",
        vec![Promotion {
            kind: EntryKind::Decision,
            summary: "checkout queries must use orders_archive_idx".into(),
            body: "seq scan on orders_archive was the bottleneck".into(),
        }],
    )?;
    println!("archived journal entry : {}", report.journal_id);
    println!("promoted codex entries  : {:?}\n", report.promoted);

    let briefing = g.brief("builder", "slow checkout performance", 8)?;
    print!("{}", briefing.markdown);

    // Verify the promoted fact once a human confirms it.
    if let Some(cid) = report.promoted.first() {
        g.verify(*cid, "team-review", Some("confirmed in PR 42".into()))?;
        println!("\nverified {}", cid);
    }

    println!("\nstore lives at {}", root.display());
    Ok(())
}
