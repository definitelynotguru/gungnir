//! CLI surface. Thin over the [`Gungnir`] facade; all behavior lives there.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::conflicts::{ConflictId, ConflictVerb};
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
        /// Entity names this entry is about (comma-separated or repeated).
        #[arg(long, value_delimiter = ',')]
        entities: Vec<String>,
        /// Target layer: codex (default) or journal
        #[arg(long, default_value = "codex")]
        into: String,
        /// Fuse embedding similarity into conflict detection (requires the
        /// `embed` feature).
        #[arg(long)]
        semantic: bool,
    },

    /// Show one entry from any layer.
    Get { id: EntryId },

    /// List entries in a layer.
    Ls {
        #[arg(long, default_value = "codex")]
        layer: String,
        #[arg(long)]
        agent: Option<String>,
        /// Only unverified entries (the review queue's membership).
        #[arg(long)]
        unreviewed: bool,
    },

    /// Propose durable facts from a transcript into the agent's journal,
    /// behind the extraction gate. The backend is $GUNGNIR_EXTRACTOR (a
    /// command honoring the contract in docs/INTEGRATIONS.md, or
    /// `heuristic`); unset makes this a no-op. Every proposal is written
    /// unverified with source_tool=extract, journal-only, evidence-backed
    /// unless --allow-unbacked, secret-denylisted, volume-capped, and
    /// conflict-scanned like any write.
    Extract {
        /// Session id or entry id to extract claims from.
        #[arg(long)]
        from: Option<String>,
        /// Transcript file; each line is cited as hashed file evidence.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Literal transcript text (unbacked; needs --allow-unbacked).
        #[arg(long)]
        text: Option<String>,
        /// The agent whose journal receives accepted proposals.
        #[arg(long)]
        agent: String,
        /// Permit proposals citing nothing durable. They still land
        /// `unverified`; no flag makes them trusted.
        #[arg(long)]
        allow_unbacked: bool,
        /// Gate verdicts only; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Max writes this run (default 20; hard ceiling 1000).
        #[arg(long)]
        cap: Option<usize>,
        /// Fuse embedding similarity into conflict detection (requires the
        /// `embed` feature).
        #[arg(long)]
        semantic: bool,
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
        /// Return only verified hits. When none verify, prints an explicit
        /// empty verdict and exits 2 — a machine-checkable abstention.
        #[arg(long)]
        require_verified: bool,
        /// Emit JSON: {hits, coverage, abstained}.
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },

    /// Mark an entry verified, or revalidate its evidence files.
    Verify {
        /// Entry to verify (or, with --revalidate, to check). Omit with
        /// --revalidate --all.
        id: Option<EntryId>,
        /// Who is vouching. Defaults to "human"; not accepted with
        /// --revalidate, which records nothing.
        #[arg(long)]
        verifier: Option<String>,
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
        /// The entry to mark contradicted.
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
        /// Fuse embedding similarity into conflict detection (requires the
        /// `embed` feature).
        #[arg(long)]
        semantic: bool,
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
        /// Fuse embedding similarity into conflict detection (requires the
        /// `embed` feature).
        #[arg(long)]
        semantic: bool,
    },

    /// List conflict candidates; `resolve` closes one with a verb.
    Conflicts {
        /// Include resolved and dismissed candidates.
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        cmd: Option<ConflictsCmd>,
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
        /// List only verified facts. When none verify, the briefing's
        /// abstention line fires and the command exits 2.
        #[arg(long)]
        require_verified: bool,
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },

    /// Audit log: one entry's history, or a chronological scan of events.
    ///
    /// `gungnir log <id>` prints the entry's verification log and its
    /// revises lineage. `gungnir log [--since <t>] [--agent <a>]
    /// [--layer <l>]` scans every partition chronologically — creation and
    /// verification events alike.
    Log {
        /// Entry id. Omit to scan events instead.
        id: Option<EntryId>,
        /// Only events at or after this instant (RFC3339). With an id,
        /// filters the verification records shown.
        #[arg(long)]
        since: Option<String>,
        /// Scan only: actor filter — entry agent for creations, verifier
        /// for transitions.
        #[arg(long)]
        agent: Option<String>,
        /// Scan only: codex, journal, or scratch.
        #[arg(long)]
        layer: Option<String>,
    },

    /// What our beliefs were at two instants: added, verified, superseded,
    /// contradicted, and rolled-back entries between <t1> and <t2>.
    Diff {
        /// Earlier instant (RFC3339).
        t1: String,
        /// Later instant (RFC3339).
        t2: String,
        /// Restrict both snapshots to entries matching this topic.
        #[arg(long)]
        topic: Option<String>,
        /// Restrict to one layer: codex, journal, or scratch.
        #[arg(long)]
        layer: Option<String>,
    },

    /// Vouching and supersession lineage of one belief: an id, or a topic
    /// resolved to its best match.
    Blame { id_or_query: String },

    /// The human review ritual: unverified (and machine-extracted) beliefs
    /// grouped by partition and project, with the verbs to act on them.
    Review {
        /// Restrict to one layer: codex, journal, or scratch.
        #[arg(long)]
        layer: Option<String>,
    },

    /// Entity index: every entity name with its entry count, or the entries
    /// under one name.
    Entities {
        /// Optional entity name to list entries for.
        name: Option<String>,
        /// Restrict to one layer: codex, journal, or scratch.
        #[arg(long)]
        layer: Option<String>,
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
enum ConflictsCmd {
    /// Close a conflict candidate with one resolution verb.
    Resolve {
        /// The candidate id, printed by `gungnir conflicts`.
        id: ConflictId,
        /// The new entry is the newer truth: link it as the revision of the
        /// conflicting entry, so current views hide the old claim.
        #[arg(long)]
        supersede: bool,
        /// The verified entry is wrong: mark it contradicted by the new one.
        #[arg(long)]
        contradict: bool,
        /// Not actually a conflict: close the candidate, touch nothing else.
        #[arg(long)]
        dismiss: bool,
        /// Who is resolving, recorded on the candidate.
        #[arg(long, default_value = "human")]
        actor: String,
    },
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

/// Exit codes: 0 success, 1 error, 2 an explicit abstention verdict
/// (`--require-verified` matched no verified knowledge).
pub const EXIT_ABSTAINED: i32 = 2;

pub fn run_from_env() -> i32 {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

fn parse_instant(raw: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&chrono::Utc))
        .map_err(|e| Error::Invalid(format!("bad timestamp '{raw}': {e}")))
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

/// First 8 chars of a digest, enough to identify drift at a glance.
/// Char-bounded, not byte-bounded: sha256 is a free string field and a
/// hand-edited value must not panic the report.
fn short(sha: &str) -> &str {
    let mut end = 8.min(sha.len());
    while !sha.is_char_boundary(end) {
        end -= 1;
    }
    &sha[..end]
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

/// Extraction verdicts on stdout: what landed (or would), what the gate
/// refused and why, and what the cap dropped. Conflicts and warnings are
/// advisory — stderr, never exit codes, same as every other write.
fn print_extract(outcome: &crate::ExtractOutcome) {
    if outcome.accepted.is_empty() && outcome.refused.is_empty() && outcome.skipped == 0 {
        println!("nothing to extract");
        return;
    }
    let verb = if outcome.dry_run {
        "would write"
    } else {
        "wrote"
    };
    for (entry, conflicts) in &outcome.accepted {
        println!(
            "{} {}  \"{}\"  (unverified, extracted)",
            verb, entry.id, entry.summary
        );
        for c in conflicts {
            eprintln!(
                "conflict candidate {}: \"{}\" vs verified \"{}\" ({}) — `gungnir conflicts` to resolve",
                c.id, c.new_summary, c.conflicting_summary, c.layer
            );
        }
    }
    for r in &outcome.refused {
        // Sensitive verdicts may carry the secret they refused on; the
        // reason names the pattern, so the text stays redacted.
        let text = if r.sensitive {
            "[redacted]"
        } else {
            &r.summary
        };
        println!("refused  \"{text}\" — {}", r.reason);
    }
    if outcome.skipped > 0 {
        println!(
            "skipped  {} proposal(s) (volume cap or duplicate)",
            outcome.skipped
        );
    }
    for w in &outcome.warnings {
        eprintln!("warning: {w}");
    }
}

/// Advisory only: the write already landed, so candidates and detection
/// warnings go to stderr and never change exit codes or the id on stdout.
fn note_conflicts(conflicts: &[crate::conflicts::ConflictRecord], warnings: &[String]) {
    for c in conflicts {
        eprintln!(
            "conflict candidate {}: \"{}\" vs verified \"{}\" ({}) — `gungnir conflicts` to resolve",
            c.id, c.new_summary, c.conflicting_summary, c.layer
        );
    }
    for w in warnings {
        eprintln!("warning: {w}");
    }
}

fn dispatch(cli: Cli) -> Result<i32> {
    use Cmd as C;
    let g = gng(&cli.root)?;
    let mut code = 0;
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
            entities,
            into,
            semantic,
        } => {
            let mut e = crate::Entry::new(&agent, kind, summary);
            e.body = body;
            e.project_id = project;
            e.review_of = review_of;
            e.entities = entities;
            let layer = match layer_of(&into)? {
                crate::layout::CODEX => crate::gungnir::Layer::Codex,
                _ => crate::gungnir::Layer::Journal { agent: &agent },
            };
            let outcome = if semantic {
                #[cfg(feature = "embed")]
                {
                    let emb = embedder_for(&g)?;
                    g.add_semantic(e, layer, emb.as_ref())?
                }
                #[cfg(not(feature = "embed"))]
                {
                    embedder_for(&g)?;
                    unreachable!("--semantic errors without the embed feature")
                }
            } else {
                g.add(e, layer)?
            };
            note_conflicts(&outcome.conflicts, &outcome.warnings);
            println!("{}", outcome.id);
        }

        C::Get { id } => {
            let store = g.locate(id)?.ok_or(Error::NotFound(id))?;
            let e = store.require(id)?;
            println!("{}  {}  {:?}  [{}]", e.id, e.kind, e.verification, e.agent);
            println!("{}", e.summary);
            if let Some(tool) = &e.source_tool {
                match &e.extracted_by {
                    Some(backend) => println!(
                        "origin: {} via {}",
                        crate::validate::one_line(tool),
                        crate::validate::one_line(backend)
                    ),
                    None => println!("origin: {}", crate::validate::one_line(tool)),
                }
            }
            let mentions = e.entity_mentions();
            if !mentions.is_empty() {
                println!(
                    "entities: {}",
                    mentions
                        .iter()
                        .map(|m| crate::validate::one_line(m))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !e.body.is_empty() {
                println!("\n{}", e.body);
            }
        }

        C::Ls {
            layer,
            agent,
            unreviewed,
        } => {
            if unreviewed {
                // The review queue's contract, not a bare unverified scan:
                // belief kinds only, and never a chain tail already
                // superseded — session archives are bookkeeping, not
                // review items.
                let l = layer_of(&layer)?;
                // Partition names use the sanitized agent — journal dirs
                // always do. The target store's own scan keeps the skipped
                // warnings the ordinary listing reports.
                let (partition, skipped) = if l == crate::layout::CODEX {
                    (crate::layout::CODEX.to_string(), g.codex()?.scan()?.skipped)
                } else {
                    let a = agent
                        .as_deref()
                        .ok_or_else(|| Error::Invalid("ls journal requires --agent".into()))?;
                    // The partition name is the resolved dir's name —
                    // hashed component, or the legacy truncated dir when
                    // that's where this agent's journal lives.
                    let store = g.journal(a)?;
                    let dir_name = store
                        .root()
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| crate::layout::sanitize_component(a));
                    (format!("{l}/{dir_name}"), store.scan()?.skipped)
                };
                let mut all = g.all_entries()?;
                all.retain(|e| e.partition == partition);
                for item in crate::audit::review_queue(&all) {
                    let e = &item.entry;
                    let extracted = if item.extracted { "  [extracted]" } else { "" };
                    println!(
                        "{}  {}\t{}\t{}{}",
                        e.id, e.kind, e.summary, e.agent, extracted
                    );
                }
                warn_skipped(&skipped);
                return Ok(0);
            }
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
                let extracted = if e.source_tool.is_some() {
                    "  [extracted]"
                } else {
                    ""
                };
                println!(
                    "{}  {}\t{}\t{}{}",
                    e.id, e.kind, e.summary, e.agent, extracted
                );
            }
            warn_skipped(&scan.skipped);
        }

        C::Extract {
            from,
            file,
            text,
            agent,
            allow_unbacked,
            dry_run,
            cap,
            semantic,
        } => {
            let source_count = from.is_some() as u8 + file.is_some() as u8 + text.is_some() as u8;
            if source_count != 1 {
                return Err(Error::Invalid(
                    "extract needs exactly one of --from, --file, --text".into(),
                ));
            }
            let Some(extractor) = crate::extract::from_env()? else {
                println!(
                    "extraction unavailable: no backend configured — set ${} to a command or `heuristic`",
                    crate::extract::EXTRACTOR_ENV
                );
                return Ok(0);
            };
            let source = match (from, file, text) {
                // Session ids are ULIDs too, so shape alone can't tell
                // them from entry ids — resolve against the store.
                (Some(id), None, None) => match id.parse::<EntryId>() {
                    Ok(id) if g.locate(id)?.is_some() => {
                        crate::extract::ExtractSource::Entry { id }
                    }
                    _ => crate::extract::ExtractSource::Session { id },
                },
                (None, Some(path), None) => crate::extract::ExtractSource::File { path },
                (None, None, Some(text)) => crate::extract::ExtractSource::Text { text },
                _ => unreachable!("exactly one source enforced above"),
            };
            let units = g.extract_units(&source, &agent)?;
            let cap = cap
                .map(|c| c.clamp(1, 1000))
                .unwrap_or(crate::extract::DEFAULT_CAP);
            let opts = crate::extract::ExtractOptions {
                allow_unbacked,
                dry_run,
                cap,
            };
            let outcome = if semantic {
                #[cfg(feature = "embed")]
                {
                    let emb = embedder_for(&g)?;
                    g.extract(
                        &units,
                        &agent,
                        extractor.as_ref(),
                        &opts,
                        Some(emb.as_ref()),
                    )?
                }
                #[cfg(not(feature = "embed"))]
                {
                    embedder_for(&g)?;
                    unreachable!("--semantic errors without the embed feature")
                }
            } else {
                g.extract(&units, &agent, extractor.as_ref(), &opts, None)?
            };
            print_extract(&outcome);
        }

        C::Recall {
            query,
            layer,
            agent,
            current,
            as_of,
            semantic,
            require_verified,
            json,
            limit,
        } => {
            let mut q = Query::new(&query, limit);
            if current {
                q = q.current();
            }
            if require_verified {
                q = q.require_verified();
            }
            if let Some(raw) = as_of {
                q = q.as_of(parse_instant(&raw)?);
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
            let abstained = require_verified && out.coverage.verified == 0;
            if json {
                let mut doc = serde_json::to_value(&out)?;
                doc["abstained"] = serde_json::Value::Bool(abstained);
                println!("{}", serde_json::to_string_pretty(&doc)?);
            } else {
                for h in &out.hits {
                    let extracted = if h.entry.source_tool.is_some() {
                        "  [extracted]"
                    } else {
                        ""
                    };
                    println!(
                        "{:.3}  {}  {}{}",
                        h.score, h.entry.id, h.entry.summary, extracted
                    );
                }
                if out.hits.is_empty() {
                    let hidden = out.coverage.hidden_superseded + out.coverage.hidden_rolled_back;
                    if out.coverage.total == 0 {
                        println!("(no knowledge on file)");
                    } else if q.limit == 0 && out.coverage.total_matched() > 0 {
                        println!(
                            "({} matched but the result limit is 0)",
                            out.coverage.total_matched()
                        );
                    } else if out.coverage.contradicted > 0 {
                        println!("(only contradicted knowledge matched this topic)");
                    } else if require_verified
                        && out.coverage.verified == 0
                        && out.coverage.unverified > 0
                    {
                        println!("(only unverified knowledge matched this topic)");
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
                if abstained {
                    println!(
                        "no verified knowledge covers this topic ({} matched, 0 verified)",
                        out.coverage.total_matched()
                    );
                }
            }
            if abstained {
                code = EXIT_ABSTAINED;
            }
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
                if !evidence.is_empty() {
                    return Err(Error::Invalid(
                        "--evidence only applies when verifying, not --revalidate".into(),
                    ));
                }
                if verifier.is_some() || note.is_some() {
                    return Err(Error::Invalid(
                        "--verifier/--note only apply when verifying, not --revalidate".into(),
                    ));
                }
                if id.is_some() && all {
                    return Err(Error::Invalid("pass an entry id or --all, not both".into()));
                }
                let report = match (id, all) {
                    (_, true) => g.revalidate_all()?,
                    (Some(id), false) => crate::RevalidateReport {
                        checks: g.revalidate(id)?,
                        unreadable: Vec::new(),
                    },
                    (None, false) => {
                        return Err(Error::Invalid(
                            "--revalidate needs an entry id or --all".into(),
                        ))
                    }
                };
                let mut intact = 0usize;
                for c in &report.checks {
                    match (&c.actual_sha256, &c.read_error) {
                        (Some(_), _) if c.intact() => intact += 1,
                        (_, Some(e)) => {
                            println!("UNREADABLE {} {} ({e})", c.entry, c.path)
                        }
                        (Some(actual), _) => println!(
                            "DRIFT   {} {} expected {} got {}",
                            c.entry,
                            c.path,
                            short(&c.expected_sha256),
                            short(actual)
                        ),
                        (None, None) => println!("MISSING {} {}", c.entry, c.path),
                    }
                }
                for path in &report.unreadable {
                    println!("UNREADABLE {}", path.display());
                }
                let drifted = report.checks.iter().filter(|c| !c.intact()).count();
                let failed = drifted + report.unreadable.len();
                println!("{intact} intact, {failed} drifted, missing, or unreadable");
                if failed > 0 {
                    return Err(Error::Drift(failed));
                }
            } else {
                let id = id.ok_or_else(|| Error::Invalid("verify needs an entry id".into()))?;
                if all {
                    return Err(Error::Invalid(
                        "--all only applies with --revalidate".into(),
                    ));
                }
                let verifier = verifier.unwrap_or_else(|| "human".into());
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
            semantic,
        } => {
            let outcome = if semantic {
                #[cfg(feature = "embed")]
                {
                    let emb = embedder_for(&g)?;
                    g.supersede_semantic(id, &agent, summary, body, emb.as_ref())?
                }
                #[cfg(not(feature = "embed"))]
                {
                    embedder_for(&g)?;
                    unreachable!("--semantic errors without the embed feature")
                }
            } else {
                g.supersede(id, &agent, summary, body)?
            };
            note_conflicts(&outcome.conflicts, &outcome.warnings);
            println!("{}", outcome.id);
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
            semantic,
        } => {
            let outcome = if semantic {
                #[cfg(feature = "embed")]
                {
                    let emb = embedder_for(&g)?;
                    g.promote_semantic(from, &agent, kind, summary, body, emb.as_ref())?
                }
                #[cfg(not(feature = "embed"))]
                {
                    embedder_for(&g)?;
                    unreachable!("--semantic errors without the embed feature")
                }
            } else {
                g.promote(from, &agent, kind, summary, body)?
            };
            note_conflicts(&outcome.conflicts, &outcome.warnings);
            println!("{}", outcome.id);
        }

        C::Conflicts { all, cmd } => match cmd {
            None => {
                let records = if all {
                    g.all_conflicts()?
                } else {
                    g.open_conflicts()?
                };
                if records.is_empty() {
                    println!("(no {}conflict candidates)", if all { "" } else { "open " });
                }
                for c in records {
                    println!(
                        "{}  {:<22}  {}  vs  {}  [{}]",
                        c.id, c.status, c.new_entry, c.conflicting_entry, c.layer
                    );
                    println!("    new:    \"{}\"", c.new_summary);
                    println!("    claims: \"{}\"", c.conflicting_summary);
                }
            }
            Some(ConflictsCmd::Resolve {
                id,
                supersede,
                contradict,
                dismiss,
                actor,
            }) => {
                let verb = match (supersede, contradict, dismiss) {
                    (true, false, false) => ConflictVerb::Supersede,
                    (false, true, false) => ConflictVerb::Contradict,
                    (false, false, true) => ConflictVerb::Dismiss,
                    _ => {
                        return Err(Error::Invalid(
                            "pick exactly one of --supersede, --contradict, --dismiss".into(),
                        ))
                    }
                };
                let rec = g.resolve_conflict(id, verb, &actor)?;
                println!("{} resolved: {}", rec.id, rec.status);
            }
        },

        C::Brief {
            agent,
            task,
            semantic,
            require_verified,
            limit,
        } => {
            let mut q = Query::new(&task, limit);
            if require_verified {
                q = q.require_verified();
            }
            let b = if semantic {
                #[cfg(feature = "embed")]
                {
                    let e = embedder_for(&g)?;
                    g.brief_with_semantic(&agent, &q, e.as_ref())?
                }
                #[cfg(not(feature = "embed"))]
                {
                    embedder_for(&g)?;
                    unreachable!("--semantic errors without the embed feature")
                }
            } else {
                g.brief_with(&agent, &q)?
            };
            print!("{}", b.markdown);
            if require_verified && b.codex_coverage.verified + b.journal_coverage.verified == 0 {
                code = EXIT_ABSTAINED;
            }
            warn_skipped(&b.skipped);
        }

        C::Log {
            id,
            since,
            agent,
            layer,
        } => {
            let since_t = since.as_deref().map(parse_instant).transpose()?;
            match id {
                Some(id) => {
                    if agent.is_some() || layer.is_some() {
                        return Err(Error::Invalid(
                            "log <id> takes only --since; --agent/--layer apply to the scan".into(),
                        ));
                    }
                    let all = g.all_entries()?;
                    let report = crate::audit::entry_report(id, &all).ok_or(Error::NotFound(id))?;
                    print!("{}", crate::audit::format_report(&report, since_t));
                }
                None => {
                    let all = g.all_entries()?;
                    let all = match layer {
                        Some(l) => {
                            let l = scan_layer(&l)?;
                            all.into_iter()
                                .filter(|e| crate::audit::in_layer(&e.partition, l))
                                .collect()
                        }
                        None => all,
                    };
                    let events = crate::audit::scan_events(&all, since_t, agent.as_deref());
                    print!("{}", crate::audit::format_events(&events));
                }
            }
        }

        C::Diff {
            t1,
            t2,
            topic,
            layer,
        } => {
            let from = parse_instant(&t1)?;
            let to = parse_instant(&t2)?;
            if to < from {
                return Err(Error::Invalid("diff needs t1 <= t2".into()));
            }
            let mut all = g.all_entries()?;
            if let Some(l) = layer {
                let l = scan_layer(&l)?;
                all.retain(|e| crate::audit::in_layer(&e.partition, l));
            }
            let snap_from = crate::audit::snapshot(&all, from, topic.as_deref());
            let snap_to = crate::audit::snapshot(&all, to, topic.as_deref());
            let diff = crate::audit::snapshot_diff(&snap_from, &snap_to);
            print!(
                "{}",
                crate::audit::format_diff(&diff, from, to, topic.as_deref())
            );
        }

        C::Blame { id_or_query } => {
            let all = g.all_entries()?;
            let located = crate::audit::resolve(&all, &id_or_query)?;
            let report = crate::audit::entry_report(located.entry.id, &all)
                .ok_or(Error::NotFound(located.entry.id))?;
            print!("{}", crate::audit::format_blame(&report));
        }

        C::Review { layer } => {
            let mut all = g.all_entries()?;
            if let Some(l) = layer {
                let l = scan_layer(&l)?;
                all.retain(|e| crate::audit::in_layer(&e.partition, l));
            }
            let queue = crate::audit::review_queue(&all);
            print!("{}", crate::audit::format_review(&queue));
        }

        C::Entities { name, layer } => {
            let mut all = g.all_entries()?;
            if let Some(l) = layer {
                let l = scan_layer(&l)?;
                all.retain(|e| crate::audit::in_layer(&e.partition, l));
            }
            print!("{}", crate::audit::format_entities(&all, name.as_deref()));
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
                note_conflicts(&report.conflicts, &report.warnings);
                if report.already_ended {
                    println!("already ended (archive {})", report.journal_id);
                } else {
                    println!("archived {}", report.journal_id);
                }
            }
        },
    }
    Ok(code)
}

/// Layer filter for the audit scans: codex, journal, or scratch.
fn scan_layer(raw: &str) -> Result<&'static str> {
    match raw {
        crate::layout::CODEX => Ok(crate::layout::CODEX),
        crate::layout::JOURNAL => Ok(crate::layout::JOURNAL),
        crate::layout::SCRATCH => Ok(crate::layout::SCRATCH),
        other => Err(Error::Invalid(format!(
            "unknown layer '{other}' (codex|journal|scratch)"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn timestamps_reject_garbage_and_accept_rfc3339() {
        let err = parse_instant("not-rfc3339").unwrap_err().to_string();
        assert!(err.contains("bad timestamp"), "{err}");
        assert!(parse_instant("2026-08-01T00:00:00Z").is_ok());
    }

    fn verify_cli(root: &std::path::Path, cmd: Cmd) -> Cli {
        Cli {
            root: Some(root.to_path_buf()),
            cmd,
        }
    }

    #[test]
    fn verify_revalidate_reports_drift_and_rejects_verify_flags() {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("adr.md");
        std::fs::write(&doc, "decision: use postgres").unwrap();

        let g = Gungnir::open(dir.path()).unwrap();
        let e = crate::Entry::new("ops", EntryKind::Decision, "use postgres");
        g.codex().unwrap().create(&e).unwrap();
        g.verify_with_evidence(e.id, "team", None, std::slice::from_ref(&doc))
            .unwrap();
        drop(g);

        let revalidate = || Cmd::Verify {
            id: Some(e.id),
            verifier: None,
            note: None,
            evidence: Vec::new(),
            revalidate: true,
            all: false,
        };

        // Intact: exit path stays clean.
        dispatch(verify_cli(dir.path(), revalidate())).unwrap();

        // Drift: the CLI contract is Error::Drift → non-zero exit.
        std::fs::write(&doc, "decision: use sqlite").unwrap();
        match dispatch(verify_cli(dir.path(), revalidate())) {
            Err(Error::Drift(1)) => {}
            other => panic!("expected Error::Drift(1), got {other:?}"),
        }

        // Verification flags are rejected, never silently dropped.
        for cmd in [
            Cmd::Verify {
                id: Some(e.id),
                verifier: None,
                note: None,
                evidence: vec![doc.clone()],
                revalidate: true,
                all: false,
            },
            Cmd::Verify {
                id: Some(e.id),
                verifier: Some("team-review".into()),
                note: None,
                evidence: Vec::new(),
                revalidate: true,
                all: false,
            },
            Cmd::Verify {
                id: Some(e.id),
                verifier: None,
                note: Some("confirmed".into()),
                evidence: Vec::new(),
                revalidate: true,
                all: false,
            },
        ] {
            match dispatch(verify_cli(dir.path(), cmd)) {
                Err(Error::Invalid(_)) => {}
                other => panic!("expected Error::Invalid, got {other:?}"),
            }
        }
    }

    #[test]
    fn revalidate_all_counts_drifted_and_unreadable_entries() {
        let dir = tempfile::tempdir().unwrap();
        let doc = dir.path().join("adr.md");
        std::fs::write(&doc, "decision: use postgres").unwrap();

        let g = Gungnir::open(dir.path()).unwrap();
        let drifted = crate::Entry::new("ops", EntryKind::Decision, "use postgres");
        g.codex().unwrap().create(&drifted).unwrap();
        g.verify_with_evidence(drifted.id, "team", None, std::slice::from_ref(&doc))
            .unwrap();
        let unreadable = crate::Entry::new("ops", EntryKind::Decision, "use sqlite");
        g.codex().unwrap().create(&unreadable).unwrap();
        let unreadable_path = g.codex().unwrap().path_for(unreadable.id);
        drop(g);
        std::fs::write(&unreadable_path, "not an entry file").unwrap();

        let all = Cmd::Verify {
            id: None,
            verifier: None,
            note: None,
            evidence: Vec::new(),
            revalidate: true,
            all: true,
        };

        // One drifted file plus one corrupt entry file → Drift(2), never
        // a swallowed error or a zero exit.
        std::fs::write(&doc, "decision: use mysql").unwrap();
        match dispatch(verify_cli(dir.path(), all)) {
            Err(Error::Drift(2)) => {}
            other => panic!("expected Error::Drift(2), got {other:?}"),
        }

        // --all without --revalidate is still rejected.
        let all_no_revalidate = Cmd::Verify {
            id: Some(drifted.id),
            verifier: None,
            note: None,
            evidence: Vec::new(),
            revalidate: false,
            all: true,
        };
        match dispatch(verify_cli(dir.path(), all_no_revalidate)) {
            Err(Error::Invalid(_)) => {}
            other => panic!("expected Error::Invalid, got {other:?}"),
        }
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
