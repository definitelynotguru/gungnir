//! Non-destructive rollback. No file is ever deleted.
//!
//! Contract: walk the `revises` chain backward from the target, find the
//! first verified ancestor, mark every intermediate rolled back, then write
//! a new rollback entry pointing at that ancestor.

use crate::entry::{Entry, EntryKind, VerificationState};
use crate::id::EntryId;
use crate::{Error, Result, Store};

#[derive(Clone, Debug)]
pub struct RollbackPlan {
    pub target: EntryId,
    /// First verified ancestor the chain restores to.
    pub ancestor: EntryId,
    /// Target plus everything between it and the ancestor; all get marked
    /// rolled back. Never contains the ancestor.
    pub intermediates: Vec<EntryId>,
}

/// Compute the plan without touching the store.
///
/// Errors when the target itself is verified (nothing to undo) or when no
/// verified ancestor exists anywhere up the chain.
pub fn plan(store: &Store, target: EntryId) -> Result<RollbackPlan> {
    let mut intermediates = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut cur = target;

    loop {
        if !visited.insert(cur) {
            return Err(Error::Invalid(format!("revises cycle at {cur}")));
        }
        let entry = store.require(cur)?;
        match entry.verification {
            VerificationState::Verified => {
                if cur == target {
                    return Err(Error::Invalid(
                        "target is already verified; nothing to roll back".into(),
                    ));
                }
                return Ok(RollbackPlan {
                    target,
                    ancestor: cur,
                    intermediates,
                });
            }
            _ => {
                intermediates.push(cur);
                match entry.revises {
                    Some(prev) => cur = prev,
                    None => {
                        return Err(Error::Invalid(format!(
                            "no verified ancestor up the chain from {target}"
                        )))
                    }
                }
            }
        }
    }
}

/// Execute a plan: mark intermediates rolled back, write the rollback entry.
/// Returns the rollback entry id. Crash between steps leaves a consistent
/// store (rolled-back markers and the rollback entry are independent).
pub fn apply(store: &Store, plan: &RollbackPlan, agent: &str) -> Result<EntryId> {
    for id in &plan.intermediates {
        let mut entry = store.require(*id)?;
        entry.mark_rolled_back(agent);
        store.update(&entry)?;
    }

    let ancestor = store.require(plan.ancestor)?;
    let n = plan.intermediates.len();
    let mut summary = format!(
        "rolled back {n} entr{}; restored state from {}",
        if n == 1 { "y" } else { "ies" },
        ancestor.id
    );
    if summary.len() > crate::validate::MAX_SUMMARY_LEN {
        summary.truncate(crate::validate::MAX_SUMMARY_LEN);
    }

    let mut rb = Entry::new(agent, EntryKind::Rollback, summary);
    rb.revises = Some(plan.ancestor);
    rb.project_id = ancestor.project_id.clone();
    rb.body = format!(
        "# Rollback of {}\n\nRolled back:\n{}",
        plan.target,
        plan.intermediates
            .iter()
            .map(|id| format!("- {id}\n"))
            .collect::<String>()
    );
    store.create(&rb)?;
    Ok(rb.id)
}

/// Plan + apply in one call.
pub fn rollback(store: &Store, target: EntryId, agent: &str) -> Result<EntryId> {
    apply(store, &plan(store, target)?, agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(store: &Store, agent: &str, summaries: &[&str]) -> Vec<EntryId> {
        let mut ids = Vec::new();
        for (i, s) in summaries.iter().enumerate() {
            let mut e = Entry::new(agent, EntryKind::Decision, *s);
            if i > 0 {
                e.revises = Some(ids[i - 1]);
            }
            store.create(&e).unwrap();
            ids.push(e.id);
        }
        ids
    }

    #[test]
    fn rolls_back_to_first_verified_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ids = chain(&store, "a", &["v1", "v2", "v3"]);

        // v1 verified; v2, v3 unverified. Rolling back v3 must mark v2+v3
        // rolled back and write a rollback entry revising v1.
        {
            let mut v1 = store.require(ids[0]).unwrap();
            v1.verify("human", None);
            store.update(&v1).unwrap();
        }

        let rb_id = rollback(&store, ids[2], "agent").unwrap();
        assert_eq!(
            store.require(ids[1]).unwrap().verification,
            VerificationState::RolledBack
        );
        assert_eq!(
            store.require(ids[2]).unwrap().verification,
            VerificationState::RolledBack
        );
        let rb = store.require(rb_id).unwrap();
        assert_eq!(rb.kind, EntryKind::Rollback);
        assert_eq!(rb.revises, Some(ids[0]));
        // All files still exist.
        for id in &ids {
            assert!(store.exists(*id).unwrap());
        }
    }

    #[test]
    fn errors_without_verified_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ids = chain(&store, "a", &["v1", "v2"]);
        assert!(matches!(
            rollback(&store, ids[1], "agent"),
            Err(Error::Invalid(_))
        ));
        // Store untouched.
        assert_ne!(
            store.require(ids[1]).unwrap().verification,
            VerificationState::RolledBack
        );
    }

    #[test]
    fn verified_target_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ids = chain(&store, "a", &["only"]);
        let mut e = store.require(ids[0]).unwrap();
        e.verify("human", None);
        store.update(&e).unwrap();
        assert!(matches!(
            rollback(&store, ids[0], "agent"),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn detects_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ids = chain(&store, "a", &["v1", "v2"]);
        // Forge a cycle by hand-editing v1 to revise v2.
        let path = store.path_for(ids[0]);
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace("revises:", "xrevises:")).unwrap();
        let mut v1 = store.require(ids[0]).unwrap();
        v1.revises = Some(ids[1]);
        store.update(&v1).unwrap();

        assert!(matches!(plan(&store, ids[1]), Err(Error::Invalid(_))));
    }
}
