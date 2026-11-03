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
/// `enforce_refs` gates the existence checks (`revises`, `review_of`,
/// `contradicted_by`, evidence refs, body `[[id]]` links). Creates must
/// pass them; updates skip them because references rot underneath a
/// persisted entry — a journal layer or a scratch session cited at write
/// time can be deleted later, and an entry must not become unwritable
/// (unverifiable, unfixable) for a link it made when the target existed.
/// Shape rules (review_of presence, caps, entity syntax) always apply.
pub fn validate_entry(entry: &Entry, exists: Exists<'_>, enforce_refs: bool) -> Result<()> {
    if entry.summary.len() > MAX_SUMMARY_LEN {
        return Err(crate::Error::Invalid(format!(
            "summary is {} chars; cap is {MAX_SUMMARY_LEN}",
            entry.summary.len()
        )));
    }

    // Fields the audit printers render as one line may not carry their own
    // markup: no newlines or control characters in the values an operator
    // reads back.
    if has_control(&entry.summary) {
        return Err(crate::Error::Invalid(
            "summary is a single printable line; no control characters".into(),
        ));
    }
    if has_control(&entry.agent) {
        return Err(crate::Error::Invalid(
            "agent is a single printable name; no control characters".into(),
        ));
    }
    if let Some(project) = &entry.project_id {
        if has_control(project) {
            return Err(crate::Error::Invalid(
                "project_id is a single printable line; no control characters".into(),
            ));
        }
    }
    for rec in &entry.verification_log {
        if has_control(&rec.verifier) {
            return Err(crate::Error::Invalid(
                "verifier is a single printable name; no control characters".into(),
            ));
        }
    }

    if entry.kind == EntryKind::Review && entry.review_of.is_none() {
        return Err(crate::Error::Invalid(
            "review entries require review_of".into(),
        ));
    }

    for name in declared_names(entry) {
        if !valid_entity_name(name) {
            return Err(crate::Error::Invalid(format!(
                "entity names are 1..{MAX_ENTITY_LEN} printable chars; got '{name}'"
            )));
        }
    }

    for ev in &entry.evidence {
        if let Evidence::File { excerpt, .. } = ev {
            if excerpt.len() > MAX_EXCERPT_LEN {
                return Err(crate::Error::Invalid(format!(
                    "evidence excerpt is {} chars; cap is {MAX_EXCERPT_LEN}",
                    excerpt.len()
                )));
            }
        }
    }

    if !enforce_refs {
        return Ok(());
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
        if let Evidence::Ref { id } = ev {
            if !exists(*id)? {
                return Err(crate::Error::Invalid(format!(
                    "evidence ref points at missing entry {id}"
                )));
            }
        }
    }

    // `[[<entry-id>]]` wikilinks must resolve like evidence refs.
    // `[[name]]` targets that are not ids are entity mentions — checked
    // by the shape rule above, not here.
    for id in entry.wikilink_ids() {
        if !exists(id)? {
            return Err(crate::Error::Invalid(format!(
                "body wikilink [[{id}]] points at missing entry"
            )));
        }
    }

    Ok(())
}

/// Existence checks for the references an update *introduces*: any id in
/// `new` that was not already on `old`. Stored references are grandfathered
/// — a link valid at write time must not brick updates when its target's
/// partition is later deleted (scratch teardown, gitignored journals) —
/// but a caller may not smuggle a fresh dangling reference into a write.
pub fn validate_new_refs(new: &Entry, old: &Entry, exists: Exists<'_>) -> Result<()> {
    let old_ids: std::collections::HashSet<EntryId> = reference_ids(old);
    if let VerificationState::Contradicted { by } = new.verification {
        let was = matches!(old.verification, VerificationState::Contradicted { by: b } if b == by);
        if !was && !exists(by)? {
            return Err(crate::Error::Invalid(format!(
                "contradicted_by points at missing entry {by}"
            )));
        }
    }
    for id in reference_ids(new) {
        if !old_ids.contains(&id) && !exists(id)? {
            return Err(crate::Error::Invalid(format!(
                "update adds reference {id} pointing at missing entry"
            )));
        }
    }
    Ok(())
}

/// Every id an entry's reference fields point at: revises, review_of,
/// evidence refs, and `[[id]]` body wikilinks. (`contradicted_by` sits on
/// the verification state and is checked beside its own error.)
fn reference_ids(entry: &Entry) -> std::collections::HashSet<EntryId> {
    let mut out: std::collections::HashSet<EntryId> = entry
        .evidence
        .iter()
        .filter_map(|ev| match ev {
            Evidence::Ref { id } => Some(*id),
            _ => None,
        })
        .collect();
    out.extend(entry.wikilink_ids());
    if let Some(prev) = entry.revises {
        out.insert(prev);
    }
    if let Some(target) = entry.review_of {
        out.insert(target);
    }
    out
}

/// The entity names a caller declares in frontmatter, validated as stored.
/// `entity_mentions` is the index view — it trims, lowercases, and drops
/// empties — so checking it both lets blank declarations slip and rejects
/// valid names whose normalized form differs (`İ` lowers to two chars).
/// Body `[[name]]` spans are not checked here: a span that fails
/// `valid_entity_name` is prose filtered by `entity_mentions`, not a claim.
fn declared_names(entry: &Entry) -> &[String] {
    &entry.entities
}

/// Shape checks for an update, grandfathering values already on record.
/// Rules only govern what the write changes: an entry stored before a
/// rule existed must still accept state transitions (verify, contradict,
/// rollback) and other fields' edits without a rewrite of its history.
/// Reference existence lives in [`validate_new_refs`].
pub fn validate_update(new: &Entry, old: &Entry) -> Result<()> {
    if new.summary != old.summary && new.summary.len() > MAX_SUMMARY_LEN {
        return Err(crate::Error::Invalid(format!(
            "summary is {} chars; cap is {MAX_SUMMARY_LEN}",
            new.summary.len()
        )));
    }
    if new.summary != old.summary && has_control(&new.summary) {
        return Err(crate::Error::Invalid(
            "summary is a single printable line; no control characters".into(),
        ));
    }
    if new.agent != old.agent && has_control(&new.agent) {
        return Err(crate::Error::Invalid(
            "agent is a single printable name; no control characters".into(),
        ));
    }
    if new.project_id != old.project_id {
        if let Some(project) = &new.project_id {
            if has_control(project) {
                return Err(crate::Error::Invalid(
                    "project_id is a single printable line; no control characters".into(),
                ));
            }
        }
    }
    for rec in &new.verification_log {
        if !old.verification_log.contains(rec) && has_control(&rec.verifier) {
            return Err(crate::Error::Invalid(
                "verifier is a single printable name; no control characters".into(),
            ));
        }
    }
    if (new.kind != old.kind || new.review_of != old.review_of)
        && new.kind == EntryKind::Review
        && new.review_of.is_none()
    {
        return Err(crate::Error::Invalid(
            "review entries require review_of".into(),
        ));
    }
    let old_names: std::collections::HashSet<&String> = old.entities.iter().collect();
    for name in declared_names(new) {
        if !old_names.contains(name) && !valid_entity_name(name) {
            return Err(crate::Error::Invalid(format!(
                "entity names are 1..{MAX_ENTITY_LEN} printable chars; got '{name}'"
            )));
        }
    }
    for ev in &new.evidence {
        if old.evidence.contains(ev) {
            continue;
        }
        if let Evidence::File { excerpt, .. } = ev {
            if excerpt.len() > MAX_EXCERPT_LEN {
                return Err(crate::Error::Invalid(format!(
                    "evidence excerpt is {} chars; cap is {MAX_EXCERPT_LEN}",
                    excerpt.len()
                )));
            }
        }
    }
    Ok(())
}

/// Names are single-line printable text, 1..MAX_ENTITY_LEN *characters*.
/// No control characters or link delimiters — a name that can escape into
/// the entity index or the audit output can't carry its own markup.
/// Whether `name` is a legal entity name; also used by `entry.rs` to
/// filter body-derived `[[name]]` mentions — a `[[` span in free text is
/// prose, not an entity claim, so it drops out rather than failing writes.
pub fn valid_entity_name(name: &str) -> bool {
    let name = name.trim();
    !name.is_empty()
        && name.chars().count() <= MAX_ENTITY_LEN
        && name
            .chars()
            .all(|c| !c.is_control() && c != '[' && c != ']')
}

fn has_control(text: &str) -> bool {
    text.chars().any(char::is_control)
}

/// Collapse control characters to spaces — the sanitize form of the
/// single-line rule, used where a value must render on one line and the
/// caller may be working with text stored before the rule existed.
pub fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
