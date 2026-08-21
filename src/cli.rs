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
        #[arg(long)] agent: String,
        #[arg(long)] kind: EntryKind,
        #[arg(long)] summary: String,
        #[arg(long, default_value = "")] body: String,
        #[arg(long)] project: Option<String>,
        /// Target layer: codex (default) or journal
        #[arg(long, default_value = "codex")] into: String,
    },

    /// Show one entry from any layer.
    Get { id: EntryId },

    /// List entries in a layer.
    Ls {
        #[arg(long, default_value = "codex")] layer: String,
        #[arg(long)] agent: Option<String>,
    },

    /// Keyword recall within a layer.
    Recall {
        query: String,
        #[arg(long, default_value = "codex")] layer: String,
        #[arg(long)] agent: Option<String>,
        #[arg(long, default_value_t = 10)] limit: usize,
    },

    /// Mark an entry verified.
    Verify {
        id: EntryId,
        #[arg(long, default_value = "human")] verifier: String,
        #[arg(long)] note: Option<String>,
    },

    /// Write a revision of an entry (supersession chain).
    Supersede {
        id: EntryId,
        #[arg(long)] agent: String,
        #[arg(long)] summary: String,
        #[arg(long, default_value = "")] body: String,
    },

    /// Non-destructive rollback to the first verified ancestor.
    Rollback {
        id: EntryId,
        #[arg(long)] agent: String,
    },

    /// Copy a finding into the Codex with provenance linked to `from`.
    Promote {
        from: EntryId,
        #[arg(long)] agent: String,
        #[arg(long, default_value = "decision")] kind: EntryKind,
        #[arg(long)] summary: String,
        #[arg(long, default_value = "")] body: String,
    },

    /// Compile the pre-task briefing for an agent.
    Brief {
        #[arg(long)] agent: String,
        task: String,
        #[arg(long, default_value_t = 8)] limit: usize,
    },

    /// Working-session lifecycle.
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },

    /// Run the MCP server on stdio (for Claude Code, Cursor, etc.).
    Mcp,
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Begin a working session; prints its id.
    Start {
        #[arg(long)] agent: String,
        task: String,
    },
    /// Record an observation into the session's scratch.
    Obs {
        session_id: String,
        #[arg(long)] agent: String,
        text: String,
    },
    /// Record an attempt (and whether it worked) into scratch.
    Attempt {
        session_id: String,
        #[arg(long)] agent: String,
        text: String,
        #[arg(long = "ok")] succeeded: bool,
    },
    /// Archive scratch into the Journal and clear it.
    End {
        /// Session id printed by `session start`.
        session_id: String,
        #[arg(long)] agent: String,
        #[arg(long)] summary: String,
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

fn layer_of(name: &str) -> Result<&'static str> {
    match name {
        "codex" => Ok(crate::layout::CODEX),
        "journal" => Ok(crate::layout::JOURNAL),
        other => Err(Error::Invalid(format!("unknown layer '{other}' (codex|journal)"))),
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    use Cmd as C;
    let g = gng(&cli.root)?;
    match cli.cmd {
        C::Init => println!("initialized {}", g.root().display()),

        C::Add { agent, kind, summary, body, project, into } => {
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
                    let a = agent.as_deref().ok_or_else(|| {
                        Error::Invalid("ls journal requires --agent".into())
                    })?;
                    g.journal(a)?.entries()?
                }
            };
            for e in entries {
                println!("{}  {}\t{}\t{}", e.id, e.kind, e.summary, e.agent);
            }
        }

        C::Recall { query, layer, agent, limit } => {
            let hits = match layer_of(&layer)? {
                crate::layout::CODEX => g.recall_layer(crate::gungnir::Layer::Codex, &Query::new(&query, limit))?,
                _ => {
                    let a = agent.as_deref().ok_or_else(|| {
                        Error::Invalid("recall journal requires --agent".into())
                    })?;
                    g.recall_layer(crate::gungnir::Layer::Journal { agent: a }, &Query::new(&query, limit))?
                }
            };
            for h in hits {
                println!("{:.3}  {}  {}", h.score, h.entry.id, h.entry.summary);
            }
        }

        C::Verify { id, verifier, note } => {
            g.verify(id, &verifier, note)?;
            println!("verified {id}");
        }

        C::Supersede { id, agent, summary, body } => {
            let new_id = g.supersede(id, &agent, summary, body)?;
            println!("{new_id}");
        }

        C::Rollback { id, agent } => {
            let rb = g.rollback(id, &agent)?;
            println!("rollback entry: {rb}");
        }

        C::Promote { from, agent, kind, summary, body } => {
            let id = g.promote(from, &agent, kind, summary, body)?;
            println!("{id}");
        }

        C::Brief { agent, task, limit } => {
            let b = g.brief(&agent, &task, limit)?;
            print!("{}", b.markdown);
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
            SessionCmd::Obs { session_id, agent, text } => {
                let s = crate::Session {
                    id: session_id,
                    agent,
                    task: String::new(),
                    started_at: chrono::Utc::now(),
                };
                let id = g.add_observation(&s, text)?;
                println!("{id}");
            }
            SessionCmd::Attempt { session_id, agent, text, succeeded } => {
                let s = crate::Session {
                    id: session_id,
                    agent,
                    task: String::new(),
                    started_at: chrono::Utc::now(),
                };
                let id = g.add_attempt(&s, text, succeeded)?;
                println!("{id}");
            }
            SessionCmd::End { session_id, agent, summary } => {
                let s = crate::Session {
                    id: session_id,
                    agent,
                    task: String::new(),
                    started_at: chrono::Utc::now(),
                };
                let report = g.end_session(&s, summary, vec![])?;
                println!("archived {}", report.journal_id);
            }
        },
    }
    Ok(())
}
