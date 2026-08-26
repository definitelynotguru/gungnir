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
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },

    /// Mark an entry verified.
    Verify {
        id: EntryId,
        #[arg(long, default_value = "human")]
        verifier: String,
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

fn gng(root: &Option<PathBuf>) -> Result<Gungnir> {
    Gungnir::open(layout::resolve_root(root.clone()))
}

/// Rebuild a session handle from its persisted identity. The task text is
/// not recoverable here; it only feeds briefing headers, which `brief`
/// rebuilds from its own argument.
fn session_handle(id: String, agent: String) -> crate::Session {
    crate::Session {
        id,
        agent,
        task: String::new(),
        started_at: chrono::Utc::now(),
    }
}

fn layer_of(name: &str) -> Result<&'static str> {
    match name {
        "codex" => Ok(crate::layout::CODEX),
        "journal" => Ok(crate::layout::JOURNAL),
        other => Err(Error::Invalid(format!(
            "unknown layer '{other}' (codex|journal)"
        ))),
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    use Cmd as C;
    let g = gng(&cli.root)?;
    match cli.cmd {
        C::Init => println!("initialized {}", g.root().display()),

        C::Add {
            agent,
            kind,
            summary,
            body,
            project,
            into,
        } => {
            let mut e = crate::Entry::new(&agent, kind, summary);
            e.body = body;
            e.project_id = project;
            let store = match layer_of(&into)? {
                crate::layout::CODEX => g.codex()?,
                _ => g.journal(&agent)?,
            };
            store.create(&e)?;
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
            let entries = match layer_of(&layer)? {
                crate::layout::CODEX => g.codex()?.entries()?,
                _ => {
                    let a = agent
                        .as_deref()
                        .ok_or_else(|| Error::Invalid("ls journal requires --agent".into()))?;
                    g.journal(a)?.entries()?
                }
            };
            for e in entries {
                println!("{}  {}\t{}\t{}", e.id, e.kind, e.summary, e.agent);
            }
        }

        C::Recall {
            query,
            layer,
            agent,
            current,
            as_of,
            limit,
        } => {
            let mut q = Query::new(&query, limit);
            if current {
                q = q.current();
            }
            if let Some(raw) = as_of {
                let t = chrono::DateTime::parse_from_rfc3339(&raw)
                    .map_err(|e| Error::Invalid(format!("bad --as-of: {e}")))?
                    .with_timezone(&chrono::Utc);
                q = q.as_of(t);
            }
            let out = match layer_of(&layer)? {
                crate::layout::CODEX => g.search_layer(crate::gungnir::Layer::Codex, &q)?,
                _ => {
                    let a = agent
                        .as_deref()
                        .ok_or_else(|| Error::Invalid("recall journal requires --agent".into()))?;
                    g.search_layer(crate::gungnir::Layer::Journal { agent: a }, &q)?
                }
            };
            for h in &out.hits {
                println!("{:.3}  {}  {}", h.score, h.entry.id, h.entry.summary);
            }
            if out.hits.is_empty() && out.coverage.total_visible() == 0 {
                println!("(no knowledge covers this topic)");
            } else {
                println!(
                    "coverage: {} verified, {} unverified, {} contradicted ({} superseded, {} rolled back hidden)",
                    out.coverage.verified,
                    out.coverage.unverified,
                    out.coverage.contradicted,
                    out.coverage.hidden_superseded,
                    out.coverage.hidden_rolled_back
                );
            }
        }

        C::Verify { id, verifier, note } => {
            g.verify(id, &verifier, note)?;
            println!("verified {id}");
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

        C::Brief { agent, task, limit } => {
            let b = g.brief(&agent, &task, limit)?;
            print!("{}", b.markdown);
        }

        C::Stats { agent, json } => {
            let r = g.stats(agent.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
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
            }
        }

        C::Mcp => {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let mut out = std::io::stdout().lock();
            crate::mcp::Server::new(g).serve(&mut input, &mut out)?;
        }

        C::Session { cmd } => match cmd {
            SessionCmd::Start { agent, task } => {
                let s = g.start_session(agent, task);
                println!("{}", s.id);
            }
            SessionCmd::Obs {
                session_id,
                agent,
                text,
            } => {
                let s = session_handle(session_id, agent);
                let id = g.add_observation(&s, text)?;
                println!("{id}");
            }
            SessionCmd::Attempt {
                session_id,
                agent,
                text,
                succeeded,
            } => {
                let s = session_handle(session_id, agent);
                let id = g.add_attempt(&s, text, succeeded)?;
                println!("{id}");
            }
            SessionCmd::End {
                session_id,
                agent,
                summary,
            } => {
                let s = session_handle(session_id, agent);
                let report = g.end_session(&s, summary, vec![])?;
                println!("archived {}", report.journal_id);
            }
        },
    }
    Ok(())
}
