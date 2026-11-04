//! Briefing compilation: turn recall hits into the short context payload an
//! agent reads before starting a task. Pure function over hits; no I/O.

use std::collections::HashSet;

use crate::entry::EntryKind;
use crate::gungnir::PeerCoverage;
use crate::id::EntryId;
use crate::recall::{self, Coverage, Hit};
use crate::store::Skipped;

#[derive(Clone, Debug, Default)]
pub struct Briefing {
    pub markdown: String,
    pub codex_hits: Vec<Hit>,
    pub journal_hits: Vec<Hit>,
    /// Topic-matched coverage per layer, kept for machine-readable callers:
    /// abstention is `codex_coverage.verified + journal_coverage.verified == 0`.
    pub codex_coverage: Coverage,
    pub journal_coverage: Coverage,
    /// Files that failed to parse while scanning either layer.
    pub skipped: Vec<Skipped>,
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
    pub codex_skipped: Vec<Skipped>,
    pub journal_skipped: Vec<Skipped>,
    /// Cross-journal topic counts, excluding the requesting agent. Counts
    /// only: peer summaries and bodies never reach the briefing.
    pub peers: PeerCoverage,
    /// Open conflict candidates touching this briefing's hits.
    pub open_conflicts: usize,
}

fn label(hit: &Hit, superseded: &HashSet<EntryId>, task_tokens: &[String]) -> String {
    let mut tags = Vec::new();
    match hit.state {
        recall::StateAt::Verified => tags.push("verified"),
        recall::StateAt::Contradicted => tags.push("CONTRADICTED"),
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
/// Relevance compares stemmed tokens on both sides — the same vocabulary
/// the scorer sees — so "latency" meets "latenci" and "us" no longer
/// swallows "status" the way a substring match would.
fn excerpt(body: &str, task_tokens: &[String]) -> Option<String> {
    let relevant = |l: &&str| {
        let line_tokens = crate::recall::tokenize(l);
        task_tokens.iter().any(|t| line_tokens.contains(t))
    };
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

/// The empty-hits line for one layer, named by what the coverage counters
/// actually hold. Under `--require-verified` unverified/contradicted
/// matches are filtered out of hits; without it, hits can only be empty
/// when every match was hidden — either way "nothing matched" must not
/// contradict the coverage line that prints below it.
fn empty_hits_line(none: &str, cov: &Coverage) -> String {
    if cov.total == 0 {
        return none.to_string();
    }
    let mut states = Vec::new();
    if cov.unverified > 0 {
        states.push("unverified");
    }
    if cov.contradicted > 0 {
        states.push("contradicted");
    }
    if cov.hidden_superseded + cov.hidden_rolled_back > 0 {
        states.push("superseded or rolled back");
    }
    if states.is_empty() {
        return "- nothing on file matched this topic\n".to_string();
    }
    format!("- matching entries were filtered: {}\n", states.join(", "))
}

/// Assemble the briefing. Coverage drives the abstention signal: when no
/// verified fact matched the topic in either layer, say so outright — and
/// say whether nothing matched or nothing verified.
pub fn compile(input: BriefingInput) -> Briefing {
    let task_tokens = crate::recall::tokenize(&input.task);
    let mut md = format!("# Briefing\n\nTask: {}\n", input.task);

    md.push_str("\n## Shared knowledge (Codex)\n");
    if input.codex_hits.is_empty() {
        md.push_str(&empty_hits_line(
            "- nothing on file\n",
            &input.codex_coverage,
        ));
    } else {
        for h in &input.codex_hits {
            md.push_str(&label(h, &input.codex_superseded, &task_tokens));
            md.push('\n');
        }
    }

    md.push_str("\n## Your prior attempts (Journal)\n");
    if input.journal_hits.is_empty() {
        md.push_str(&empty_hits_line(
            "- no prior attempts on file\n",
            &input.journal_coverage,
        ));
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
    if input.peers.agents_attempted > 0 {
        let p = &input.peers;
        let agents = if p.agents_attempted == 1 {
            "agent".to_string()
        } else {
            "agents".to_string()
        };
        md.push_str(&format!(
            "- Peers: {} other {agents} attempted this topic; {} failed\n",
            p.agents_attempted, p.agents_failed
        ));
    }
    if input.codex_coverage.verified == 0 && input.journal_coverage.verified == 0 {
        // Contradicted and hidden (superseded, rolled back) matches count:
        // the briefing lists them above, so "nothing matched" must not
        // contradict its own output.
        let c = input.codex_coverage;
        let j = input.journal_coverage;
        let matched_any = c
            .total_visible()
            .saturating_add(c.contradicted)
            .saturating_add(c.hidden_superseded)
            .saturating_add(c.hidden_rolled_back)
            .saturating_add(j.total_visible())
            .saturating_add(j.contradicted)
            .saturating_add(j.hidden_superseded)
            .saturating_add(j.hidden_rolled_back)
            > 0;
        if matched_any {
            md.push_str(
                "\nEntries matched this task but none are verified. Proceed with caution.\n",
            );
        } else {
            md.push_str("\nNo knowledge matched this task. Proceed with caution.\n");
        }
    }

    let contradicted = input.codex_coverage.contradicted + input.journal_coverage.contradicted;
    if contradicted > 0 {
        md.push_str(&format!(
            "\nWarning: {contradicted} contradicted fact(s) above. Do not rely on them.\n"
        ));
    }

    if input.open_conflicts > 0 {
        md.push_str(&format!(
            "\nWarning: {} unresolved conflict candidate(s) on this topic — see `gungnir conflicts`.\n",
            input.open_conflicts
        ));
    }

    let mut skipped = input.codex_skipped;
    skipped.extend(input.journal_skipped);

    Briefing {
        markdown: md,
        codex_hits: input.codex_hits,
        journal_hits: input.journal_hits,
        codex_coverage: input.codex_coverage,
        journal_coverage: input.journal_coverage,
        skipped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::Entry;
    use crate::recall::StateAt;

    fn input() -> BriefingInput {
        BriefingInput {
            task: "fix login".into(),
            codex_hits: vec![],
            journal_hits: vec![],
            codex_superseded: HashSet::new(),
            journal_superseded: HashSet::new(),
            codex_coverage: Coverage::default(),
            journal_coverage: Coverage::default(),
            codex_skipped: vec![],
            journal_skipped: vec![],
            peers: PeerCoverage::default(),
            open_conflicts: 0,
        }
    }

    #[test]
    fn empty_briefing_says_so_and_abstains() {
        let b = compile(input());
        assert!(b.markdown.contains("nothing on file"));
        assert!(b.markdown.contains("no prior attempts"));
        assert!(b.markdown.contains("No knowledge matched this task"));
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
        e.contradict(EntryId::generate(), "reviewer", None);
        let mut i = input();
        i.codex_hits = vec![Hit {
            entry: e,
            state: StateAt::Contradicted,
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
            state: StateAt::Unverified,
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
            state: StateAt::Unverified,
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
            state: StateAt::Unverified,
            score: 1.0,
        }];
        i.journal_hits = vec![Hit {
            entry: e,
            state: StateAt::Unverified,
            score: 1.0,
        }];
        i.codex_coverage.unverified = 1;
        i.journal_coverage.unverified = 1;
        let b = compile(i);
        assert!(b
            .markdown
            .contains("Entries matched this task but none are verified"));
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
            state: StateAt::Unverified,
            score: 1.0,
        }];
        i.codex_coverage.unverified = 1;
        let b = compile(i);
        assert!(b
            .markdown
            .contains("Entries matched this task but none are verified"));
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
            state: StateAt::Unverified,
            score: 1.0,
        }];
        let b = compile(i);
        assert!(
            b.markdown.contains("- attempt: added checkout cache"),
            "{}",
            b.markdown
        );
    }

    #[test]
    fn excerpt_matches_stemmed_tokens_not_substrings() {
        // "used" stems to "us"; a substring matcher would offer the
        // "status" line as relevant. Under stemmed-token matching the body
        // shares no token with the task, so no excerpt is picked.
        let mut e = Entry::new("a", EntryKind::Observation, "connection notes");
        e.body = "status update: all green".into();
        let mut i = input();
        i.task = "used connections".into();
        i.codex_hits = vec![Hit {
            entry: e,
            state: StateAt::Unverified,
            score: 1.0,
        }];
        let b = compile(i);
        assert!(!b.markdown.contains("> status"), "{}", b.markdown);
    }

    #[test]
    fn excerpt_still_fires_when_stems_agree() {
        let mut e = Entry::new("a", EntryKind::Observation, "orders report");
        e.body = "query latency on orders doubled".into();
        let mut i = input();
        i.task = "query latency".into();
        i.codex_hits = vec![Hit {
            entry: e,
            state: StateAt::Unverified,
            score: 1.0,
        }];
        let b = compile(i);
        assert!(
            b.markdown.contains("> query latency on orders doubled"),
            "{}",
            b.markdown
        );
    }
}
