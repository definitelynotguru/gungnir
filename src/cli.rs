//! CLI surface. Thin over the [`Gungnir`] facade; all behavior lives there.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::entry::EntryKind;
use crate::id::EntryId;
use crate::layout;
use crate::recall::Query;
use crate::{Error, Gungnir, Result};

#[derive(Parser)]
#[command(
    name = "gungnir",
    version,
    about = "Gungnir — local-first, markdown-native memory for AI agents"
)]
pub struct Cli {
    /// Store root (default: $GUNGNIR_ROOT or ~/.gungnir)
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the store directory structure.
    Init,

    /// Add an entry to a persistent layer.
    Add {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        kind: EntryKind,
        #[arg(long)]
        summary: String,
        #[arg(long, default_value = "")]
        body: String,
        #[arg(long)]
        project: Option<String>,
        /// The entry this review evaluates (required for --kind review)
        #[arg(long)]
        review_of: Option<EntryId>,
        /// Target layer: codex (default) or journal
        #[arg(long, default_value = "codex")]
        into: String,
    },

    /// Show one entry from any layer.
    Get { id: EntryId },

    /// List entries in a layer.
    Ls {
        #[arg(long, default_value = "codex")]
        layer: String,
        #[arg(long)]
        agent: Option<String>,
    },

    /// Keyword recall within a layer.
    Recall {
        query: String,
        #[arg(long, default_value = "codex")]
        layer: String,
        #[arg(long)]
        agent: Option<String>,
        /// Resolve supersession chains to heads; drop contradicted facts.
        #[arg(long)]
        current: bool,
        /// Evaluate facts as they stood at this instant (RFC3339).
        #[arg(long)]
        as_of: Option<String>,
        /// Fuse in embedding-similarity ranking (requires the `embed`
        /// feature; downloads a ~100MB local model on first use).
        #[arg(long)]
        semantic: bool,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },

    /// Mark an entry verified, or revalidate its evidence files.
    Verify {
        /// Entry to verify (or, with --revalidate, to check). Omit with
        /// --revalidate --all.
        id: Option<EntryId>,
        #[arg(long, default_value = "human")]
        verifier: String,
        #[arg(long)]
        note: Option<String>,
        /// Record file evidence at verify time: hash the file's content
        /// and keep an excerpt. Repeatable.
        #[arg(long)]
        evidence: Vec<PathBuf>,
        /// Re-hash the entry's evidence files instead of verifying; flag
        /// any whose content drifted since citation. Read-only.
        #[arg(long)]
        revalidate: bool,
        /// With --revalidate: check file evidence on every entry.
        #[arg(long)]
        all: bool,
    },

    /// Mark an entry contradicted by another entry.
    Contradict {
        id: EntryId,
        /// The entry that contradicts `id`
        #[arg(long)]
        by: EntryId,
        #[arg(long, default_value = "human")]
        verifier: String,
        /// Free-text reason recorded in the verification log
        #[arg(long)]
        note: Option<String>,
    },

    /// Write a revision of an entry (supersession chain).
    Supersede {
        id: EntryId,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        summary: String,
        #[arg(long, default_value = "")]
        body: String,
    },

    /// Non-destructive rollback to the first verified ancestor.
    Rollback {
        id: EntryId,
        #[arg(long)]
        agent: String,
    },

    /// Copy a finding into the Codex with provenance linked to `from`.
    Promote {
        from: EntryId,
        #[arg(long)]
        agent: String,
        #[arg(long, default_value = "decision")]
        kind: EntryKind,
        #[arg(long)]
        summary: String,
        #[arg(long, default_value = "")]
        body: String,
    },

    /// Compile the pre-task briefing for an agent.
    Brief {
        #[arg(long)]
        agent: String,
        task: String,
        /// Fuse in embedding-similarity ranking (requires the `embed`
        /// feature; downloads a ~100MB local model on first use).
        #[arg(long)]
        semantic: bool,
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },

    /// Working-session lifecycle.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },

    /// Memory health summary.
    Stats {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long)]
        json: bool,
        /// Cross-journal coverage for this topic: how many other agents'
        /// journals touch it and how many recorded a failed attempt.
        /// Counts only; no peer content is exposed.
        #[arg(long)]
        topic: Option<String>,
    },

    /// Run the MCP server on stdio (for Claude Code, Cursor, etc.).
    Mcp,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Begin a working session; prints its id.
    Start {
        #[arg(long)]
        agent: String,
        task: String,
    },
    /// Record an observation into the session's scratch.
    Obs {
        session_id: String,
        #[arg(long)]
        agent: String,
        text: String,
    },
    /// Record an attempt (and whether it worked) into scratch.
    Attempt {
        session_id: String,
        #[arg(long)]
        agent: String,
        text: String,
        #[arg(long = "ok")]
        succeeded: bool,
    },
    /// Archive scratch into the Journal and clear it.
    End {
        /// Session id printed by `session start`.
        session_id: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        summary: String,
    },
}

pub fn run_from_env() -> i32 {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn parse_as_of(raw: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| Error::Invalid(format!("bad --as-of: {e}")))
}

fn gng(root: &Option<PathBuf>) -> Result<Gungnir> {
    Gungnir::open(layout::resolve_root(root.clone()))
}

/// Rebuild a session handle from its persisted identity. `session start`
/// writes `scratch/<id>/.session.yaml`, so a rebuilt handle recovers the
/// real task text instead of archiving an empty `# Task`.
fn session_handle(g: &Gungnir, id: String, agent: String) -> Result<crate::Session> {
    if let Some(s) = g.load_session(&id)? {
        return Ok(s);
    }
    Ok(crate::Session {
        id,
        agent,
        task: String::new(),
        started_at: chrono::Utc::now(),
    })
}

/// Warn on stderr about files a read skipped, keeping stdout clean for
/// pipelines.
fn warn_skipped(skipped: &[crate::store::Skipped]) {
    for s in skipped {
        eprintln!(
            "warning: skipped unreadable entry {}: {}",
            s.path.display(),
            s.error
        );
    }
}

fn layer_of(name: &str) -> Result<&'static str> {
    layout::parse_layer_name(Some(name)).map_err(Error::Invalid)
}

/// First 8 hex chars of a digest, enough to identify drift at a glance.
fn short(sha: &str) -> &str {
    &sha[..8.min(sha.len())]
}

/// The shipped local embedder (fastembed bge-small-en-v1.5), wrapped in the
/// content-addressed vector cache under `<root>/.cache`. Boxed so tests can
/// swap in a fake: under the test-only `test-embedder` feature,
/// `GUNGNIR_TEST_EMBEDDER=1` serves the uniform-vector `Uniform` embedder
/// instead of the real model. A plain `--features embed` build compiles the
/// hook out, so the env var can never change a release binary's behavior.
#[cfg(feature = "embed")]
fn embedder_for(g: &Gungnir) -> Result<Box<dyn crate::embedding::Embedder>> {
    #[cfg(feature = "test-embedder")]
    if std::env::var("GUNGNIR_TEST_EMBEDDER").is_ok_and(|v| v == "1") {
        return Ok(Box::new(crate::embedding::Uniform));
    }
    let inner = crate::embedding::local::Fastembed::new(g.root())?;
    Ok(Box::new(crate::embedding::CachedEmbedder::new(
        inner,
        g.root(),
    )))
}

#[cfg(not(feature = "embed"))]
fn embedder_for(_g: &Gungnir) -> Result<()> {
    Err(Error::Invalid(
        "--semantic requires the `embed` feature: rebuild with `cargo build --features embed`"
            .into(),
    ))
}

fn dispatch(cli: Cli) -> Result<()> {
    use Cmd as C;
    let g = gng(&cli.root)?;
    match cli.cmd {
        C::Init => {
            g.init()?;
            println!("initialized {}", g.root().display())
        }

        C::Add {
            agent,
            kind,
            summary,
            body,
            project,
            review_of,
            into,
        } => {
            let mut e = crate::Entry::new(&agent, kind, summary);
            e.body = body;
            e.project_id = project;
            e.review_of = review_of;
            let store = match layer_of(&into)? {
                crate::layout::CODEX => g.codex()?,
                _ => g.journal(&agent)?,
            };
            // Cross-layer resolver so a journal review can cite a codex target.
            store.create_with(&e, &|id| Ok(g.locate(id)?.is_some()))?;
            println!("{}", e.id);
        }

        C::Get { id } => {
            let store = g.locate(id)?.ok_or(Error::NotFound(id))?;
            let e = store.require(id)?;
            println!("{}  {}  {:?}  [{}]", e.id, e.kind, e.verification, e.agent);
            println!("{}", e.summary);
            if !e.body.is_empty() {
                println!("\n{}", e.body);
            }
        }

        C::Ls { layer, agent } => {
            let scan = match layer_of(&layer)? {
                crate::layout::CODEX => g.codex()?.scan()?,
                _ => {
                    let a = agent
                        .as_deref()
                        .ok_or_else(|| Error::Invalid("ls journal requires --agent".into()))?;
                    g.journal(a)?.scan()?
                }
            };
            for e in &scan.entries {
                println!("{}  {}\t{}\t{}", e.id, e.kind, e.summary, e.agent);
            }
            warn_skipped(&scan.skipped);
        }

        C::Recall {
            query,
            layer,
            agent,
            current,
            as_of,
            semantic,
            limit,
        } => {
            let mut q = Query::new(&query, limit);
            if current {
                q = q.current();
            }
            if let Some(raw) = as_of {
                q = q.as_of(parse_as_of(&raw)?);
            }
            let layer_sel = match layer_of(&layer)? {
                crate::layout::CODEX => crate::gungnir::Layer::Codex,
                _ => crate::gungnir::Layer::Journal {
                    agent: agent
                        .as_deref()
                        .ok_or_else(|| Error::Invalid("recall journal requires --agent".into()))?,
                },
            };
            let out = if semantic {
                #[cfg(feature = "embed")]
                {
                    let e = embedder_for(&g)?;
                    g.search_layer_semantic(layer_sel, &q, e.as_ref())?
                }
                #[cfg(not(feature = "embed"))]
                {
                    embedder_for(&g)?;
                    unreachable!("--semantic errors without the embed feature")
                }
            } else {
                g.search_layer(layer_sel, &q)?
            };
            for h in &out.hits {
                println!("{:.3}  {}  {}", h.score, h.entry.id, h.entry.summary);
            }
            if out.hits.is_empty() {
                let hidden = out.coverage.hidden_superseded + out.coverage.hidden_rolled_back;
                if out.coverage.total == 0 {
                    println!("(no knowledge on file)");
                } else if out.coverage.contradicted > 0 {
                    println!("(only contradicted knowledge matched this topic)");
                } else if out.coverage.total_visible() == 0 && hidden > 0 {
                    println!("(no live knowledge covers this topic; {hidden} superseded or rolled back hidden)");
                } else {
                    println!("(no knowledge matched this topic)");
                }
            }
            println!(
                "coverage: {} verified, {} unverified, {} contradicted ({} superseded, {} rolled back hidden)",
                out.coverage.verified,
                out.coverage.unverified,
                out.coverage.contradicted,
                out.coverage.hidden_superseded,
                out.coverage.hidden_rolled_back
            );
            warn_skipped(&out.skipped);
        }

        C::Verify {
            id,
            verifier,
            note,
            evidence,
            revalidate,
            all,
        } => {
            if revalidate {
                let checks = match (id, all) {
                    (_, true) => g.revalidate_all()?,
                    (Some(id), false) => g.revalidate(id)?,
                    (None, false) => {
                        return Err(Error::Invalid(
                            "--revalidate needs an entry id or --all".into(),
                        ))
                    }
                };
                let mut intact = 0usize;
                for c in &checks {
                    match &c.actual_sha256 {
                        Some(_) if c.intact() => intact += 1,
                        Some(actual) => println!(
                            "DRIFT   {} {} expected {} got {}",
                            c.entry,
                            c.path,
                            short(&c.expected_sha256),
                            short(actual)
                        ),
                        None => println!("MISSING {} {}", c.entry, c.path),
                    }
                }
                let drifted = checks.iter().filter(|c| !c.intact()).count();
                println!("{intact} intact, {drifted} drifted or missing");
                if drifted > 0 {
                    return Err(Error::Drift(drifted));
                }
            } else {
                let id = id.ok_or_else(|| Error::Invalid("verify needs an entry id".into()))?;
                if all {
                    return Err(Error::Invalid(
                        "--all only applies with --revalidate".into(),
                    ));
                }
                if evidence.is_empty() {
                    g.verify(id, &verifier, note)?;
                } else {
                    g.verify_with_evidence(id, &verifier, note, &evidence)?;
                }
                println!("verified {id}");
            }
        }

        C::Contradict {
            id,
            by,
            verifier,
            note,
        } => {
            g.contradict(id, by, &verifier, note)?;
            println!("contradicted {id} by {by}");
        }

        C::Supersede {
            id,
            agent,
            summary,
            body,
        } => {
            let new_id = g.supersede(id, &agent, summary, body)?;
            println!("{new_id}");
        }

        C::Rollback { id, agent } => {
            let rb = g.rollback(id, &agent)?;
            println!("rollback entry: {rb}");
        }

        C::Promote {
            from,
            agent,
            kind,
            summary,
            body,
        } => {
            let id = g.promote(from, &agent, kind, summary, body)?;
            println!("{id}");
        }

        C::Brief {
            agent,
            task,
            semantic,
            limit,
        } => {
            let b = if semantic {
                #[cfg(feature = "embed")]
                {
                    let e = embedder_for(&g)?;
                    g.brief_semantic(&agent, &task, limit, e.as_ref())?
                }
                #[cfg(not(feature = "embed"))]
                {
                    embedder_for(&g)?;
                    unreachable!("--semantic errors without the embed feature")
                }
            } else {
                g.brief(&agent, &task, limit)?
            };
            print!("{}", b.markdown);
            warn_skipped(&b.skipped);
        }

        C::Stats { agent, json, topic } => {
            let r = g.stats(agent.as_deref())?;
            let peers = match &topic {
                Some(t) => Some(g.peer_coverage(agent.as_deref(), t)?),
                None => None,
            };
            if json {
                let mut out = serde_json::to_value(&r)?;
                if let Some(p) = &peers {
                    out["peer_coverage"] = serde_json::to_value(p)?;
                }
                println!("{}", serde_json::to_string_pretty(&out)?);
            } else {
                println!("codex entries      {}", r.codex_entries);
                println!("journal entries    {}", r.journal_entries);
                println!("scratch sessions   {}", r.scratch_sessions);
                println!("verified           {}", r.verified);
                println!("unverified         {}", r.unverified);
                println!("contradicted       {}", r.contradicted);
                println!("rolled back        {}", r.rolled_back);
                println!("superseded         {}", r.superseded);
                println!("stale (>30d)       {}", r.stale_over_30d);
                println!("verification rate  {:.0}%", r.verification_rate * 100.0);
                println!("skipped (unparseable) {}", r.skipped.len());
                if let Some(p) = &peers {
                    println!(
                        "peer coverage      {} agents attempted, {} failed ({} entries)",
                        p.agents_attempted, p.agents_failed, p.entries
                    );
                }
            }
            warn_skipped(&r.skipped);
        }

        C::Mcp => {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let mut out = std::io::stdout().lock();
            crate::mcp::Server::new(g).serve(&mut input, &mut out)?;
        }

        C::Session { cmd } => match cmd {
            SessionCmd::Start { agent, task } => {
                let s = g.start_session(agent, task)?;
                println!("{}", s.id);
            }
            SessionCmd::Obs {
                session_id,
                agent,
                text,
            } => {
                let s = session_handle(&g, session_id, agent)?;
                let id = g.add_observation(&s, text)?;
                println!("{id}");
            }
            SessionCmd::Attempt {
                session_id,
                agent,
                text,
                succeeded,
            } => {
                let s = session_handle(&g, session_id, agent)?;
                let id = g.add_attempt(&s, text, succeeded)?;
                println!("{id}");
            }
            SessionCmd::End {
                session_id,
                agent,
                summary,
            } => {
                let s = session_handle(&g, session_id, agent)?;
                let report = g.end_session(&s, summary, vec![])?;
                if report.already_ended {
                    println!("already ended (archive {})", report.journal_id);
                } else {
                    println!("archived {}", report.journal_id);
                }
            }
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn as_of_rejects_garbage_and_accepts_rfc3339() {
        let err = parse_as_of("not-rfc3339").unwrap_err().to_string();
        assert!(err.contains("bad --as-of"), "{err}");
        assert!(parse_as_of("2026-08-01T00:00:00Z").is_ok());
    }

    #[test]
    fn clap_accepts_current_and_as_of_together() {
        let cli = Cli::try_parse_from([
            "gungnir",
            "recall",
            "checkout",
            "--current",
            "--as-of",
            "2026-08-01T00:00:00Z",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Recall { current, as_of, .. } => {
                assert!(current);
                assert_eq!(as_of.as_deref(), Some("2026-08-01T00:00:00Z"));
            }
            _ => panic!("expected Recall"),
        }
    }
}
