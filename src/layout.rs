//! On-disk layout: one root, three layer partitions.
//!
//! ```text
//! <root>/codex/<id>.md            shared source of truth
//! <root>/journal/<agent>/<id>.md  private per-agent history
//! <root>/scratch/<session>/<id>.md  ephemeral per-task working memory
//! <root>/.cache/embeddings/...    derived vectors
//! ```
//!
//! Directory partitioning makes privacy a `.gitignore` line (`scratch/`,
//! `journal/`) and scratch cleanup a directory removal.

use std::path::PathBuf;

pub const CODEX: &str = "codex";
pub const JOURNAL: &str = "journal";
pub const SCRATCH: &str = "scratch";
pub const CACHE: &str = ".cache";
/// Conflict-candidate queue: one markdown record per detected conflict.
pub const CONFLICTS: &str = ".conflicts";

/// Root resolution: explicit argument beats `GUNGNIR_ROOT` beats `~/.gungnir`.
pub fn resolve_root(explicit: Option<PathBuf>) -> PathBuf {
    explicit
        .or_else(|| std::env::var("GUNGNIR_ROOT").ok().map(PathBuf::from))
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".gungnir")
        })
}

/// Reduce a free-form name (agent, session) to a safe single path component.
/// Identity lives in frontmatter; this only needs to be collision-tolerant,
/// not reversible.
pub fn sanitize_component(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = cleaned
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let capped: String = collapsed.chars().take(64).collect();
    // A cut mid-name leaves the tail ambiguous: two different long names
    // can truncate to the same dir, and a trailing separator remnant makes
    // the result not a fixed point (layer_child_names would drop it).
    // Reserve the tail for a stable hash of the full cleaned name instead
    // — injective for distinct names and self-describing.
    if collapsed.chars().count() > 64 {
        use sha2::Digest as _;
        let head: String = collapsed.chars().take(55).collect();
        let head = head.trim_end_matches('-');
        let tag = format!("{:x}", sha2::Sha256::digest(collapsed.as_bytes()));
        return format!("{head}-{}", &tag[..8]);
    }
    if capped.is_empty() {
        "anon".into()
    } else {
        capped
    }
}

/// Directory name under the scheme used before hashed truncation: a bare
/// 64-char cut, separator remnant included. Kept for reading journals and
/// scratch dirs written by older builds — new names come from
/// [`sanitize_component`], this is lookup-only.
pub fn sanitize_component_legacy(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = cleaned
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    collapsed.chars().take(64).collect()
}

/// Whether `name` is a directory component this codebase writes — under
/// either the hashed scheme or the legacy truncation. Agent dirs end up
/// here through `layer_child_names` on every scan, so the predicate must
/// accept both eras' names rather than only today's shape.
pub fn is_component_dir(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Shared layer name parser for CLI and MCP. Omitted means Codex.
pub fn parse_layer_name(raw: Option<&str>) -> Result<&'static str, String> {
    match raw {
        None | Some(CODEX) => Ok(CODEX),
        Some(JOURNAL) => Ok(JOURNAL),
        Some(other) => Err(format!("unknown layer '{other}' (codex|journal)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_component_edges() {
        assert_eq!(sanitize_component("Backend Agent #1"), "backend-agent-1");
        assert_eq!(sanitize_component("///"), "anon");
        assert_eq!(sanitize_component("ÅÄÖ"), "anon");
    }

    #[test]
    fn sanitize_component_truncation_is_collision_free() {
        // Two names identical through the 64-char cut diverge after it.
        let a = format!("{}-x", "a".repeat(63));
        let b = format!("{}-y", "a".repeat(63));
        let (da, db) = (sanitize_component(&a), sanitize_component(&b));
        assert_ne!(da, db, "truncated names must not collide");
        assert!(da.len() <= 64 && db.len() <= 64);
        // Hashed names are fixed points so scans keep them.
        assert_eq!(sanitize_component(&da), da);
        assert_eq!(sanitize_component(&db), db);
        // The same input always hashes to the same dir.
        assert_eq!(sanitize_component(&a), da);
    }

    #[test]
    fn is_component_dir_accepts_both_name_eras() {
        // Today's hashed name and the legacy truncated-with-remnant shape.
        let hashed = sanitize_component(&format!("{}-x", "a".repeat(63)));
        assert!(is_component_dir(&hashed));
        assert!(is_component_dir(&format!("{}-", "a".repeat(63))));
        assert!(!is_component_dir(""));
        assert!(!is_component_dir("Mixed-Case"));
        assert!(!is_component_dir(&"a".repeat(65)));
    }
}
