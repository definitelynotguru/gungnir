//! Write-time validation rules.
//!
//! Rules are pure: reference resolution goes through an [`Exists`] resolver
//! so the same rules govern single-store writes and cross-layer writes
//! (e.g. Codex evidence pointing at a Journal archive entry).

use crate::entry::{Evidence, EntryKind, VerificationState};
use crate::{Entry, EntryId, Result};

pub const MAX_SUMMARY_LEN: usize = 200;
pub const MAX_EXCERPT_LEN: usize = 500;

/// Resolve whether an entry id exists, across whatever scope the caller cares
/// about.
pub type Exists<'a> = &'a dyn Fn(EntryId) -> Result<bool>;

/// Validate an entry against every write-time rule except uniqueness,
/// which only the store can judge (it knows its own ids).
///
/// Used for both creates and updates; the store distinguishes them by
/// checking id presence before calling here.
pub fn validate_entry(entry: &Entry, exists: Exists<'_>) -> Result<()> {
    if entry.summary.len() > MAX_SUMMARY_LEN {
        return Err(crate::Error::Invalid(format!(
            "summary is {} chars; cap is {MAX_SUMMARY_LEN}",
            entry.summary.len()
        )));
    }

    if entry.kind == EntryKind::Review && entry.review_of.is_none() {
        return Err(crate::Error::Invalid("review entries require review_of".into()));
    }

    if let VerificationState::Contradicted { by } = entry.verification {
        if !exists(by)? {
            return Err(crate::Error::Invalid(format!(
                "contradicted_by points at missing entry {by}"
            )));
        }
    }

    if let Some(prev) = entry.revises {
        if !exists(prev)? {
            return Err(crate::Error::Invalid(format!(
                "revises points at missing entry {prev}"
            )));
        }
    }

    if let Some(target) = entry.review_of {
        if !exists(target)? {
            return Err(crate::Error::Invalid(format!(
                "review_of points at missing entry {target}"
            )));
        }
    }

    for ev in &entry.evidence {
        match ev {
            Evidence::File { excerpt, .. } => {
                if excerpt.len() > MAX_EXCERPT_LEN {
                    return Err(crate::Error::Invalid(format!(
                        "evidence excerpt is {} chars; cap is {MAX_EXCERPT_LEN}",
                        excerpt.len()
                    )));
                }
            }
            Evidence::Ref { id } => {
                if !exists(*id)? {
                    return Err(crate::Error::Invalid(format!(
                        "evidence ref points at missing entry {id}"
                    )));
                }
            }
        }
    }

    Ok(())
}
