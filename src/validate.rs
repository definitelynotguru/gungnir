//! Write-time validation rules.
//!
//! Rules are pure: reference resolution goes through an [`Exists`] resolver
//! so the same rules govern single-store writes and cross-layer writes
//! (e.g. Codex evidence pointing at a Journal archive entry).

use crate::entry::{EntryKind, Evidence, VerificationState};
use crate::{Entry, EntryId, Result};

pub const MAX_SUMMARY_LEN: usize = 200;
pub const MAX_EXCERPT_LEN: usize = 500;
pub const MAX_ENTITY_LEN: usize = 64;

/// Resolve whether an entry id exists, across whatever scope the caller cares
/// about.
pub type Exists<'a> = &'a dyn Fn(EntryId) -> Result<bool>;

/// Validate an entry against every write-time rule except uniqueness,
/// which only the store can judge (it knows its own ids).
///
/// Used for both creates and updates; the store distinguishes them by
/// checking id presence before calling here.
pub fn validate_entry(entry: &Entry, exists: Exists<'_>) -> Result<()> {
    if entry.summary.chars().count() > MAX_SUMMARY_LEN {
        return Err(crate::Error::Invalid(format!(
            "summary is {} chars; cap is {MAX_SUMMARY_LEN}",
            entry.summary.chars().count()
        )));
    }

    if entry.kind == EntryKind::Review && entry.review_of.is_none() {
        return Err(crate::Error::Invalid(
            "review entries require review_of".into(),
        ));
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
                if excerpt.chars().count() > MAX_EXCERPT_LEN {
                    return Err(crate::Error::Invalid(format!(
                        "evidence excerpt is {} chars; cap is {MAX_EXCERPT_LEN}",
                        excerpt.chars().count()
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

    for name in &entry.entities {
        let name = name.trim();
        if name.is_empty() || name.len() > MAX_ENTITY_LEN {
            return Err(crate::Error::Invalid(format!(
                "entity names are 1..{MAX_ENTITY_LEN} chars; got '{name}'"
            )));
        }
    }

    // `[[<entry-id>]]` wikilinks must resolve like evidence refs.
    // `[[name]]` targets that are not ids are entity mentions — free form.
    for id in entry.wikilink_ids() {
        if !exists(id)? {
            return Err(crate::Error::Invalid(format!(
                "body wikilink [[{id}]] points at missing entry"
            )));
        }
    }

    Ok(())
}
