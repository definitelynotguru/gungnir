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
    if capped.is_empty() {
        "anon".into()
    } else {
        capped
    }
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
}
