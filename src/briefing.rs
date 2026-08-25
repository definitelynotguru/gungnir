//! Briefing compilation: turn recall hits into the short context payload an
//! agent reads before starting a task. Pure function over hits; no I/O.

use std::collections::HashSet;

use crate::entry::{EntryKind, VerificationState};
use crate::id::EntryId;
use crate::recall::Hit;

#[derive(Clone, Debug, Default)]
pub struct Briefing {
    pub markdown: String,
    pub codex_hits: Vec<Hit>,
    pub journal_hits: Vec<Hit>,
}

fn label(hit: &Hit, superseded: &HashSet<EntryId>, task_tokens: &[String]) -> String {
    let mut tags = Vec::new();
    match hit.entry.verification {
        VerificationState::Verified => tags.push("verified"),
        VerificationState::Contradicted { .. } => tags.push("CONTRADICTED"),
        _ => {}
    }
    if superseded.contains(&hit.entry.id) {
        tags.push("superseded");
    }
    if hit.entry.kind == EntryKind::Attempt {
        tags.push("prior attempt");
    }
    let mut out = if tags.is_empty() {
        format!("- {} ({})", hit.entry.summary, hit.entry.id)
    } else {
        format!(
            "- [{}] {} ({})",
            tags.join(", "),
            hit.entry.summary,
            hit.entry.id
        )
    };
    if let Some(x) = excerpt(&hit.entry.body, task_tokens) {
        out.push_str(&format!("\n  > {x}"));
    }
    out
}

/// First body line relevant to the task, as context under the summary.
/// Transcript bullets ("- attempt: ...") win over prose echoes of the task.
fn excerpt(body: &str, task_tokens: &[String]) -> Option<String> {
    let relevant = |l: &&str| task_tokens.iter().any(|t| l.to_lowercase().contains(t));
    let lines: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let picked = lines
        .iter()
        .copied()
        .find(|l| l.starts_with("- ") && relevant(l))
        .or_else(|| lines.iter().copied().find(relevant));
    picked.map(|l| l.chars().take(160).collect())
}

/// Assemble the briefing. `codex_superseded` / `journal_superseded` are the
/// sets of entry ids that some newer entry revises; hits in those sets get
/// flagged so stale facts announce themselves.
pub fn compile(
    task: &str,
    codex_hits: Vec<Hit>,
    journal_hits: Vec<Hit>,
    codex_superseded: HashSet<EntryId>,
    journal_superseded: HashSet<EntryId>,
) -> Briefing {
    let task_tokens = crate::recall::tokenize(task);
    let mut md = format!("# Briefing\n\nTask: {task}\n");

    md.push_str("\n## Shared knowledge (Codex)\n");
    if codex_hits.is_empty() {
        md.push_str("- nothing on file for this topic\n");
    } else {
        for h in &codex_hits {
            md.push_str(&label(h, &codex_superseded, &task_tokens));
            md.push('\n');
        }
    }

    md.push_str("\n## Your prior attempts (Journal)\n");
    if journal_hits.is_empty() {
        md.push_str("- no prior attempts on this topic\n");
    } else {
        for h in &journal_hits {
            md.push_str(&label(h, &journal_superseded, &task_tokens));
            md.push('\n');
        }
    }

    let contradicted = codex_hits
        .iter()
        .chain(journal_hits.iter())
        .filter(|h| matches!(h.entry.verification, VerificationState::Contradicted { .. }))
        .count();
    if contradicted > 0 {
        md.push_str(&format!(
            "\nWarning: {contradicted} contradicted fact(s) above. Do not rely on them.\n"
        ));
    }

    Briefing {
        markdown: md,
        codex_hits,
        journal_hits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;

    #[test]
    fn empty_briefing_says_so() {
        let b = compile("fix login", vec![], vec![], HashSet::new(), HashSet::new());
        assert!(b.markdown.contains("nothing on file"));
        assert!(b.markdown.contains("no prior attempts"));
    }

    #[test]
    fn superseded_and_contradicted_are_flagged() {
        let mut e = Entry::new("a", EntryKind::Decision, "deploy on fridays");
        e.contradict(EntryId::generate(), "reviewer");
        let hit = Hit {
            entry: e,
            score: 1.0,
        };

        let b = compile("deploy", vec![hit], vec![], HashSet::new(), HashSet::new());
        assert!(b.markdown.contains("CONTRADICTED"));
        assert!(b.markdown.contains("Do not rely on them"));
    }

    #[test]
    fn superseded_set_adds_marker() {
        let e = Entry::new("a", EntryKind::Decision, "use mysql");
        let mut set = HashSet::new();
        set.insert(e.id);
        let hit = Hit {
            entry: e,
            score: 1.0,
        };
        let b = compile("db", vec![hit], vec![], set, HashSet::new());
        assert!(b.markdown.contains("[superseded]"));
    }
}
