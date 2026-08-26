//! Briefing compilation: turn recall hits into the short context payload an
//! agent reads before starting a task. Pure function over hits; no I/O.

use std::collections::HashSet;

use crate::entry::{EntryKind, VerificationState};
use crate::id::EntryId;
use crate::recall::{Coverage, Hit};

#[derive(Clone, Debug, Default)]
pub struct Briefing {
    pub markdown: String,
    pub codex_hits: Vec<Hit>,
    pub journal_hits: Vec<Hit>,
}

/// Everything the compiler needs. One struct instead of seven positional
/// parameters that all happen to be optional-ish.
#[derive(Clone, Debug)]
pub struct BriefingInput {
    pub task: String,
    pub codex_hits: Vec<Hit>,
    pub journal_hits: Vec<Hit>,
    pub codex_superseded: HashSet<EntryId>,
    pub journal_superseded: HashSet<EntryId>,
    pub codex_coverage: Coverage,
    pub journal_coverage: Coverage,
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

fn coverage_line(name: &str, cov: &Coverage) -> String {
    format!(
        "- {name}: {} verified, {} unverified, {} contradicted ({} superseded and {} rolled back hidden)",
        cov.verified, cov.unverified, cov.contradicted, cov.hidden_superseded, cov.hidden_rolled_back
    )
}

/// Assemble the briefing. Coverage drives the abstention signal: when no
/// verified fact matched the topic in either layer, say so outright.
pub fn compile(input: BriefingInput) -> Briefing {
    let task_tokens = crate::recall::tokenize(&input.task);
    let mut md = format!("# Briefing\n\nTask: {}\n", input.task);

    md.push_str("\n## Shared knowledge (Codex)\n");
    if input.codex_hits.is_empty() {
        md.push_str("- nothing on file for this topic\n");
    } else {
        for h in &input.codex_hits {
            md.push_str(&label(h, &input.codex_superseded, &task_tokens));
            md.push('\n');
        }
    }

    md.push_str("\n## Your prior attempts (Journal)\n");
    if input.journal_hits.is_empty() {
        md.push_str("- no prior attempts on this topic\n");
    } else {
        for h in &input.journal_hits {
            md.push_str(&label(h, &input.journal_superseded, &task_tokens));
            md.push('\n');
        }
    }

    md.push_str("\n## Coverage\n");
    md.push_str(&coverage_line("Codex", &input.codex_coverage));
    md.push('\n');
    md.push_str(&coverage_line("Journal", &input.journal_coverage));
    md.push('\n');
    if input.codex_coverage.verified == 0 && input.journal_coverage.verified == 0 {
        md.push_str("\nNo verified knowledge covers this task. Proceed with caution.\n");
    }

    let contradicted = input.codex_coverage.contradicted + input.journal_coverage.contradicted;
    if contradicted > 0 {
        md.push_str(&format!(
            "\nWarning: {contradicted} contradicted fact(s) above. Do not rely on them.\n"
        ));
    }

    Briefing {
        markdown: md,
        codex_hits: input.codex_hits,
        journal_hits: input.journal_hits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;

    fn input() -> BriefingInput {
        BriefingInput {
            task: "fix login".into(),
            codex_hits: vec![],
            journal_hits: vec![],
            codex_superseded: HashSet::new(),
            journal_superseded: HashSet::new(),
            codex_coverage: Coverage::default(),
            journal_coverage: Coverage::default(),
        }
    }

    #[test]
    fn empty_briefing_says_so_and_abstains() {
        let b = compile(input());
        assert!(b.markdown.contains("nothing on file"));
        assert!(b.markdown.contains("no prior attempts"));
        assert!(b
            .markdown
            .contains("No verified knowledge covers this task"));
    }

    #[test]
    fn verified_coverage_skips_the_abstention_line() {
        let mut i = input();
        i.codex_coverage.verified = 2;
        let b = compile(i);
        assert!(!b.markdown.contains("No verified knowledge"));
        assert!(b.markdown.contains("2 verified"));
    }

    #[test]
    fn superseded_and_contradicted_are_flagged() {
        let mut e = Entry::new("a", EntryKind::Decision, "deploy on fridays");
        e.contradict(EntryId::generate(), "reviewer");
        let mut i = input();
        i.codex_hits = vec![Hit {
            entry: e,
            score: 1.0,
        }];
        i.codex_coverage.contradicted = 1;
        let b = compile(i);
        assert!(b.markdown.contains("CONTRADICTED"));
        assert!(b.markdown.contains("Do not rely on them"));
    }

    #[test]
    fn superseded_set_adds_marker() {
        let e = Entry::new("a", EntryKind::Decision, "use mysql");
        let mut set = HashSet::new();
        set.insert(e.id);
        let mut i = input();
        i.codex_hits = vec![Hit {
            entry: e,
            score: 1.0,
        }];
        i.codex_superseded = set;
        let b = compile(i);
        assert!(b.markdown.contains("[superseded]"));
    }

    #[test]
    fn journal_verified_prevents_abstention() {
        let e = Entry::new("a", EntryKind::Observation, "maybe cache");
        let mut i = input();
        i.codex_hits = vec![Hit {
            entry: e,
            score: 1.0,
        }];
        i.codex_coverage.unverified = 1;
        i.journal_coverage.verified = 1;
        let b = compile(i);
        assert!(!b.markdown.contains("No verified knowledge"));
    }

    #[test]
    fn both_layers_unverified_with_hits_still_abstains() {
        let e = Entry::new("a", EntryKind::Observation, "maybe cache");
        let mut i = input();
        i.codex_hits = vec![Hit {
            entry: e.clone(),
            score: 1.0,
        }];
        i.journal_hits = vec![Hit {
            entry: e,
            score: 1.0,
        }];
        i.codex_coverage.unverified = 1;
        i.journal_coverage.unverified = 1;
        let b = compile(i);
        assert!(b
            .markdown
            .contains("No verified knowledge covers this task"));
        assert!(b.markdown.contains("1 unverified"));
    }

    #[test]
    fn abstention_still_fires_when_unverified_hits_exist() {
        let e = Entry::new(
            "a",
            EntryKind::Observation,
            "maybe the cache is the problem",
        );
        let mut i = input();
        i.codex_hits = vec![Hit {
            entry: e,
            score: 1.0,
        }];
        i.codex_coverage.unverified = 1;
        let b = compile(i);
        assert!(b
            .markdown
            .contains("No verified knowledge covers this task"));
        assert!(b.markdown.contains("1 unverified"));
    }

    #[test]
    fn excerpt_surfaces_transcript_detail() {
        let mut e = Entry::new("a", EntryKind::Session, "cache helped");
        e.body = "# Task\nfix slow checkout\n\n# Transcript\n- attempt: added checkout cache [succeeded]\n".to_string();
        let mut i = input();
        i.task = "cache tuning".into();
        i.journal_hits = vec![Hit {
            entry: e,
            score: 1.0,
        }];
        let b = compile(i);
        assert!(
            b.markdown.contains("- attempt: added checkout cache"),
            "{}",
            b.markdown
        );
    }
}
