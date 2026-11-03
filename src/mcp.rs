//! MCP (Model Context Protocol) server over stdio.
//!
//! Newline-delimited JSON-RPC 2.0, as the MCP stdio transport specifies.
//! Hand-rolled against the protocol rather than an SDK: the surface needed
//! here is small (initialize, tools/list, tools/call, ping) and this avoids
//! an async runtime plus SDK API churn.
//!
//! Tool errors are reported as results with `isError: true` (per spec);
//! malformed requests get JSON-RPC error objects.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde_json::{json, Value};

use crate::entry::EntryKind;
use crate::gungnir::Session;
use crate::id::EntryId;
use crate::recall::Query;
use crate::{Error, Gungnir, Result};

const PROTOCOL_VERSION: &str = "2024-11-05";

enum LayerSel {
    Codex,
    Journal,
}

fn parse_layer(raw: Option<&str>) -> Result<LayerSel> {
    match crate::layout::parse_layer_name(raw).map_err(Error::Invalid)? {
        crate::layout::JOURNAL => Ok(LayerSel::Journal),
        _ => Ok(LayerSel::Codex),
    }
}

/// Conflict candidates a write raised, appended to the tool's normal reply
/// so agents see them without a second call.
fn conflict_note(
    base: String,
    conflicts: &[crate::conflicts::ConflictRecord],
    warnings: &[String],
) -> String {
    if conflicts.is_empty() && warnings.is_empty() {
        return base;
    }
    let mut out = base;
    for c in conflicts {
        out.push_str(&format!(
            "\nconflict candidate {}: \"{}\" vs verified \"{}\" ({}) — resolve_conflict to close",
            c.id, c.new_summary, c.conflicting_summary, c.layer
        ));
    }
    for w in warnings {
        out.push_str(&format!("\nwarning: {w}"));
    }
    out
}

/// Open sessions by id, so agents only pass a session handle between calls.
pub struct Server {
    g: Gungnir,
    sessions: HashMap<String, Session>,
    /// One embedder for the server's life: the inner model loads lazily on
    /// the first semantic call and stays resident for every later one.
    /// Boxed so tests can inject a fake (see `crate::embedding::Uniform`).
    #[cfg(feature = "embed")]
    embedder: Option<Box<dyn crate::embedding::Embedder>>,
}

impl Server {
    pub fn new(g: Gungnir) -> Self {
        Self {
            g,
            sessions: HashMap::new(),
            #[cfg(feature = "embed")]
            embedder: None,
        }
    }

    fn tools(&self) -> Value {
        fn schema(props: Value, required: &[&str]) -> Value {
            let mut s = json!({"type": "object", "properties": props});
            if !required.is_empty() {
                s["required"] = json!(required);
            }
            s
        }
        json!([
            {
                "name": "start_session",
                "description": "Begin a working session. Returns a briefing of shared facts and your prior attempts for the task.",
                "inputSchema": schema(json!({
                    "agent": {"type": "string"},
                    "task": {"type": "string"}
                }), &["agent", "task"])
            },
            {
                "name": "add_observation",
                "description": "Record an observation made during the session.",
                "inputSchema": schema(json!({
                    "session_id": {"type": "string"},
                    "agent": {"type": "string"},
                    "text": {"type": "string"}
                }), &["session_id", "agent", "text"])
            },
            {
                "name": "add_attempt",
                "description": "Record an attempt and whether it succeeded.",
                "inputSchema": schema(json!({
                    "session_id": {"type": "string"},
                    "agent": {"type": "string"},
                    "text": {"type": "string"},
                    "succeeded": {"type": "boolean"}
                }), &["session_id", "agent", "text"])
            },
            {
                "name": "end_session",
                "description": "Archive the session into your private journal and clear scratch.",
                "inputSchema": schema(json!({
                    "session_id": {"type": "string"},
                    "agent": {"type": "string"},
                    "summary": {"type": "string"}
                }), &["session_id", "agent", "summary"])
            },
            {
                "name": "recall",
                "description": "Keyword search. layer: codex (default) or journal. semantic: fuse in embedding similarity (needs the `embed` build). require_verified: only verified hits — the result always ends with a machine-readable coverage JSON line including `abstained`. as_of: evaluate history at an RFC3339 instant.",
                "inputSchema": schema(json!({
                    "query": {"type": "string"},
                    "layer": {"type": "string", "enum": ["codex", "journal"]},
                    "agent": {"type": "string"},
                    "limit": {"type": "integer"},
                    "semantic": {"type": "boolean"},
                    "require_verified": {"type": "boolean"},
                    "as_of": {"type": "string"}
                }), &["query"])
            },
            {
                "name": "brief",
                "description": "Compile the pre-task briefing without opening a session. semantic: fuse in embedding similarity (needs the `embed` build). require_verified: verified facts only; when none exist the briefing's abstention line fires.",
                "inputSchema": schema(json!({
                    "agent": {"type": "string"},
                    "task": {"type": "string"},
                    "limit": {"type": "integer"},
                    "semantic": {"type": "boolean"},
                    "require_verified": {"type": "boolean"}
                }), &["agent", "task"])
            },
            {
                "name": "verify",
                "description": "Mark an entry verified.",
                "inputSchema": schema(json!({
                    "id": {"type": "string"},
                    "verifier": {"type": "string"},
                    "note": {"type": "string"}
                }), &["id"])
            },
            {
                "name": "contradict",
                "description": "Mark an entry contradicted by another entry.",
                "inputSchema": schema(json!({
                    "id": {"type": "string"},
                    "by": {"type": "string"},
                    "verifier": {"type": "string"},
                    "note": {"type": "string"}
                }), &["id", "by"])
            },
            {
                "name": "promote",
                "description": "Copy a finding into the shared Codex with provenance linked to its source entry. kind=review makes the promotion a review of the source.",
                "inputSchema": schema(json!({
                    "from": {"type": "string"},
                    "agent": {"type": "string"},
                    "kind": {"type": "string", "enum": ["decision", "observation", "attempt", "review"]},
                    "summary": {"type": "string"},
                    "body": {"type": "string"}
                }), &["from", "agent", "summary"])
            },
            {
                "name": "supersede",
                "description": "Write a revision of an existing fact; history is preserved via the revises chain.",
                "inputSchema": schema(json!({
                    "id": {"type": "string"},
                    "agent": {"type": "string"},
                    "summary": {"type": "string"},
                    "body": {"type": "string"}
                }), &["id", "agent", "summary"])
            },
            {
                "name": "rollback",
                "description": "Non-destructively roll back a fact to its first verified ancestor.",
                "inputSchema": schema(json!({
                    "id": {"type": "string"},
                    "agent": {"type": "string"}
                }), &["id", "agent"])
            },
            {
                "name": "contradict",
                "description": "Mark an entry contradicted by another entry (the newer evidence).",
                "inputSchema": schema(json!({
                    "id": {"type": "string"},
                    "by": {"type": "string"},
                    "verifier": {"type": "string"}
                }), &["id", "by"])
            },
            {
                "name": "conflicts",
                "description": "List open conflict candidates visible to agent: codex endpoints plus the agent's own journal. all: include resolved and dismissed.",
                "inputSchema": schema(json!({
                    "agent": {"type": "string"},
                    "all": {"type": "boolean"}
                }), &["agent"])
            },
            {
                "name": "resolve_conflict",
                "description": "Close a conflict candidate visible to agent. verb: supersede (newer truth, links the revision chain), contradict (verified entry is wrong), or dismiss (not a conflict).",
                "inputSchema": schema(json!({
                    "id": {"type": "string"},
                    "verb": {"type": "string", "enum": ["supersede", "contradict", "dismiss"]},
                    "agent": {"type": "string"}
                }), &["id", "verb", "agent"])
            },
            {
                "name": "list",
                "description": "List entries in a layer. layer: codex (default) or journal.",
                "inputSchema": schema(json!({
                    "layer": {"type": "string", "enum": ["codex", "journal"]},
                    "agent": {"type": "string"},
                    "limit": {"type": "integer"}
                }), &[])
            },
            {
                "name": "get",
                "description": "Fetch one entry from any layer.",
                "inputSchema": schema(json!({
                    "id": {"type": "string"}
                }), &["id"])
            }
        ])
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<String> {
        let arg = |k: &str| args.get(k).and_then(Value::as_str);
        match name {
            "start_session" => {
                let agent = arg("agent").ok_or_else(|| Error::Invalid("agent required".into()))?;
                let task = arg("task").ok_or_else(|| Error::Invalid("task required".into()))?;
                let s = self.g.start_session(agent, task)?;
                let briefing = self.g.brief(agent, task, 8)?;
                self.sessions.insert(s.id.clone(), s.clone());
                let mut text = format!("session_id: {}\n\n{}", s.id, briefing.markdown);
                for s in &briefing.skipped {
                    text.push_str(&format!(
                        "\n(skipped unreadable entry {}: {})",
                        s.path.display(),
                        s.error
                    ));
                }
                Ok(text)
            }
            "add_observation" => {
                let s = self.session(args)?;
                let text = arg("text").ok_or_else(|| Error::Invalid("text required".into()))?;
                let id = self.g.add_observation(&s, text)?;
                Ok(id.to_string())
            }
            "add_attempt" => {
                let s = self.session(args)?;
                let text = arg("text").ok_or_else(|| Error::Invalid("text required".into()))?;
                let ok = args
                    .get("succeeded")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let id = self.g.add_attempt(&s, text, ok)?;
                Ok(id.to_string())
            }
            "end_session" => {
                let s = self.session(args)?;
                let summary =
                    arg("summary").ok_or_else(|| Error::Invalid("summary required".into()))?;
                let report = self.g.end_session(&s, summary, vec![])?;
                if report.already_ended {
                    Ok(format!("already ended (archive {})", report.journal_id))
                } else {
                    self.sessions.remove(&s.id);
                    Ok(conflict_note(
                        format!("archived {}", report.journal_id),
                        &report.conflicts,
                        &report.warnings,
                    ))
                }
            }
            "recall" => {
                let query = arg("query").ok_or_else(|| Error::Invalid("query required".into()))?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                let require_verified = args
                    .get("require_verified")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut q = Query::new(query, limit);
                if require_verified {
                    q = q.require_verified();
                }
                if let Some(raw) = arg("as_of") {
                    let t = chrono::DateTime::parse_from_rfc3339(raw)
                        .map(|t| t.with_timezone(&chrono::Utc))
                        .map_err(|e| Error::Invalid(format!("bad as_of: {e}")))?;
                    q = q.as_of(t);
                }
                let layer = match parse_layer(arg("layer"))? {
                    LayerSel::Journal => crate::gungnir::Layer::Journal {
                        agent: arg("agent").ok_or_else(|| {
                            Error::Invalid("agent required for journal recall".into())
                        })?,
                    },
                    LayerSel::Codex => crate::gungnir::Layer::Codex,
                };
                let out = if self.semantic_enabled(args)? {
                    #[cfg(feature = "embed")]
                    {
                        self.ensure_embedder()?;
                        self.g.search_layer_semantic(
                            layer,
                            &q,
                            self.embedder.as_deref().unwrap(),
                        )?
                    }
                    #[cfg(not(feature = "embed"))]
                    unreachable!("semantic_enabled errors without the embed feature")
                } else {
                    self.g.search_layer(layer, &q)?
                };
                let abstained = require_verified && out.hits.is_empty();
                let mut coverage = serde_json::to_value(out.coverage)?;
                coverage["abstained"] = Value::Bool(abstained);
                // Coverage is always emitted — the abstention signal is the
                // contract, not a side note. Last line so it parses cleanly.
                let mut hits_text = if out.hits.is_empty() {
                    "no matches".to_string()
                } else {
                    out.hits
                        .iter()
                        .map(|h| format!("{:.3}  {}  {}", h.score, h.entry.id, h.entry.summary))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                for s in &out.skipped {
                    hits_text.push_str(&format!(
                        "\n(skipped unreadable entry {}: {})",
                        s.path.display(),
                        s.error
                    ));
                }
                Ok(format!(
                    "{hits_text}\ncoverage: {}",
                    serde_json::to_string(&coverage)?
                ))
            }
            "brief" => {
                let agent = arg("agent").ok_or_else(|| Error::Invalid("agent required".into()))?;
                let task = arg("task").ok_or_else(|| Error::Invalid("task required".into()))?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize;
                let mut q = Query::new(task, limit);
                if args
                    .get("require_verified")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    q = q.require_verified();
                }
                let b = if self.semantic_enabled(args)? {
                    #[cfg(feature = "embed")]
                    {
                        self.ensure_embedder()?;
                        self.g
                            .brief_with_semantic(agent, &q, self.embedder.as_deref().unwrap())?
                    }
                    #[cfg(not(feature = "embed"))]
                    unreachable!("semantic_enabled errors without the embed feature")
                } else {
                    self.g.brief_with(agent, &q)?
                };
                let mut text = b.markdown;
                for s in &b.skipped {
                    text.push_str(&format!(
                        "\n(skipped unreadable entry {}: {})",
                        s.path.display(),
                        s.error
                    ));
                }
                Ok(text)
            }
            "verify" => {
                let id: EntryId = arg("id")
                    .ok_or_else(|| Error::Invalid("id required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad id: {e}")))?;
                let verifier = arg("verifier").unwrap_or("agent");
                self.g
                    .verify(id, verifier, arg("note").map(str::to_owned))?;
                Ok(format!("verified {id}"))
            }
            "contradict" => {
                let id: EntryId = arg("id")
                    .ok_or_else(|| Error::Invalid("id required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad id: {e}")))?;
                let by: EntryId = arg("by")
                    .ok_or_else(|| Error::Invalid("by required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad by id: {e}")))?;
                let verifier = arg("verifier").unwrap_or("agent");
                self.g
                    .contradict(id, by, verifier, arg("note").map(str::to_owned))?;
                Ok(format!("contradicted {id} by {by}"))
            }
            "promote" => {
                let from: EntryId = arg("from")
                    .ok_or_else(|| Error::Invalid("from required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad id: {e}")))?;
                let agent = arg("agent").ok_or_else(|| Error::Invalid("agent required".into()))?;
                let kind = match arg("kind") {
                    Some(k) => k.parse()?,
                    None => EntryKind::Decision,
                };
                let summary =
                    arg("summary").ok_or_else(|| Error::Invalid("summary required".into()))?;
                let outcome =
                    self.g
                        .promote(from, agent, kind, summary, arg("body").unwrap_or(""))?;
                Ok(conflict_note(
                    format!("promoted to {}", outcome.id),
                    &outcome.conflicts,
                    &outcome.warnings,
                ))
            }
            "supersede" => {
                let id: EntryId = arg("id")
                    .ok_or_else(|| Error::Invalid("id required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad id: {e}")))?;
                let agent = arg("agent").ok_or_else(|| Error::Invalid("agent required".into()))?;
                let summary =
                    arg("summary").ok_or_else(|| Error::Invalid("summary required".into()))?;
                let outcome = self
                    .g
                    .supersede(id, agent, summary, arg("body").unwrap_or(""))?;
                Ok(conflict_note(
                    format!("revision {} supersedes {id}", outcome.id),
                    &outcome.conflicts,
                    &outcome.warnings,
                ))
            }
            "conflicts" => {
                let agent = arg("agent").ok_or_else(|| Error::Invalid("agent required".into()))?;
                let all = args.get("all").and_then(Value::as_bool).unwrap_or(false);
                // Same scoping briefings apply: a candidate naming a
                // private journal endpoint stays invisible to other
                // agents, record summaries included.
                let records = self.g.conflicts_for(agent, all)?;
                if records.is_empty() {
                    return Ok(if all {
                        "(no conflict candidates)".into()
                    } else {
                        "(no open conflict candidates)".into()
                    });
                }
                Ok(records
                    .iter()
                    .map(|c| {
                        format!(
                            "{}  {}  {} vs {}  [{}]\n    new:    \"{}\"\n    claims: \"{}\"",
                            c.id,
                            c.status,
                            c.new_entry,
                            c.conflicting_entry,
                            c.layer,
                            c.new_summary,
                            c.conflicting_summary
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "resolve_conflict" => {
                let id: crate::conflicts::ConflictId = arg("id")
                    .ok_or_else(|| Error::Invalid("id required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad conflict id: {e}")))?;
                let verb = match arg("verb") {
                    Some("supersede") => crate::conflicts::ConflictVerb::Supersede,
                    Some("contradict") => crate::conflicts::ConflictVerb::Contradict,
                    Some("dismiss") => crate::conflicts::ConflictVerb::Dismiss,
                    other => {
                        return Err(Error::Invalid(format!(
                            "verb must be supersede|contradict|dismiss, got {other:?}"
                        )))
                    }
                };
                let actor = arg("agent").ok_or_else(|| Error::Invalid("agent required".into()))?;
                let rec = self.g.conflict_record(id)?;
                if !self.g.conflict_visible_to(actor, &rec)? {
                    return Err(Error::Invalid(format!("conflict not found: {id}")));
                }
                let rec = self.g.resolve_conflict(id, verb, actor)?;
                Ok(format!("{} resolved: {}", rec.id, rec.status))
            }
            "rollback" => {
                let id: EntryId = arg("id")
                    .ok_or_else(|| Error::Invalid("id required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad id: {e}")))?;
                let agent = arg("agent").ok_or_else(|| Error::Invalid("agent required".into()))?;
                let rb = self.g.rollback(id, agent)?;
                Ok(format!("rollback entry: {rb}"))
            }
            "list" => {
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
                let render = |scan: crate::store::Scan| -> String {
                    let mut text = if scan.entries.is_empty() {
                        "(empty)".to_string()
                    } else {
                        scan.entries
                            .into_iter()
                            .take(limit)
                            .map(|e| format!("{}  {}\t{}", e.id, e.kind, e.summary))
                            .collect::<Vec<_>>()
                            .join("\n")
                    };
                    for s in &scan.skipped {
                        text.push_str(&format!(
                            "\n(skipped unreadable entry {}: {})",
                            s.path.display(),
                            s.error
                        ));
                    }
                    text
                };
                match parse_layer(arg("layer"))? {
                    LayerSel::Journal => {
                        let agent = arg("agent").ok_or_else(|| {
                            Error::Invalid("agent required for journal list".into())
                        })?;
                        Ok(render(self.g.journal(agent)?.scan()?))
                    }
                    LayerSel::Codex => Ok(render(self.g.codex()?.scan()?)),
                }
            }
            "get" => {
                let id: EntryId = arg("id")
                    .ok_or_else(|| Error::Invalid("id required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad id: {e}")))?;
                let store = self.g.locate(id)?.ok_or(Error::NotFound(id))?;
                let e = store.require(id)?;
                Ok(format!("{}  {}\n{}\n\n{}", e.id, e.kind, e.summary, e.body))
            }
            other => Err(Error::Invalid(format!("unknown tool '{other}'"))),
        }
    }

    /// `semantic: true` on a tool call requests hybrid search. Without the
    /// `embed` build the honest answer is an error, not a silent keyword
    /// search dressed as semantic.
    fn semantic_enabled(&self, args: &Value) -> Result<bool> {
        let on = match args.get("semantic") {
            None => false,
            Some(v) => v.as_bool().ok_or_else(|| {
                // A mistyped flag must error rather than read as false and
                // silently downgrade a semantic request to keyword.
                Error::Invalid("`semantic` must be a boolean".into())
            })?,
        };
        #[cfg(not(feature = "embed"))]
        if on {
            return Err(Error::Invalid(
                "semantic recall requires a build with `--features embed`".into(),
            ));
        }
        Ok(on)
    }

    /// The init borrows end when this returns; call sites then hold a
    /// plain `&` on `self.embedder` alongside `&self.g`.
    #[cfg(feature = "embed")]
    fn ensure_embedder(&mut self) -> Result<()> {
        if self.embedder.is_none() {
            self.embedder = Some(Box::new(crate::embedding::CachedEmbedder::new(
                crate::embedding::local::Fastembed::new(self.g.root())?,
                self.g.root(),
            )));
        }
        Ok(())
    }

    /// Reconstruct the session handle; works across server restarts because
    /// scratch lives on disk keyed by session id and `start_session` persists
    /// the identity to `.session.yaml`.
    fn session(&self, args: &Value) -> Result<Session> {
        let arg = |k: &str| args.get(k).and_then(Value::as_str);
        let id = arg("session_id").ok_or_else(|| Error::Invalid("session_id required".into()))?;
        if let Some(s) = self.sessions.get(id) {
            return Ok(s.clone());
        }
        if let Some(mut s) = self.g.load_session(id)? {
            // The persisted identity wins; a passed agent only matters when
            // no meta exists.
            s.id = id.to_string();
            return Ok(s);
        }
        let agent = arg("agent")
            .ok_or_else(|| Error::Invalid("agent required (unknown session_id)".into()))?;
        Ok(Session {
            id: id.to_string(),
            agent: agent.to_string(),
            task: String::new(),
            started_at: chrono::Utc::now(),
        })
    }

    fn respond(&self, out: &mut impl Write, msg: Value) -> Result<()> {
        writeln!(out, "{msg}")?;
        out.flush()?;
        Ok(())
    }

    /// Serve requests until EOF on `input`.
    pub fn serve<R: BufRead>(&mut self, input: R, out: &mut impl Write) -> Result<()> {
        for line in input.lines() {
            let line = line?.trim().to_string();
            if line.is_empty() {
                continue;
            }
            let req: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    self.respond(
                        out,
                        json!({
                            "jsonrpc": "2.0", "id": null,
                            "error": {"code": -32700, "message": format!("parse error: {e}")}
                        }),
                    )?;
                    continue;
                }
            };
            let id = req.get("id").cloned();
            let method = req.get("method").and_then(Value::as_str).unwrap_or("");

            let response = match method {
                "initialize" => json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": PROTOCOL_VERSION,
                        "capabilities": {"tools": {"listChanged": false}},
                        "serverInfo": {"name": "gungnir", "version": env!("CARGO_PKG_VERSION")}
                    }
                }),
                "notifications/initialized" | "initialized" => continue,
                "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                "tools/list" => {
                    json!({"jsonrpc": "2.0", "id": id, "result": {"tools": self.tools()}})
                }
                "tools/call" => {
                    let params = req.get("params").cloned().unwrap_or(Value::Null);
                    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                    let empty = Value::Object(serde_json::Map::new());
                    let args = params.get("arguments").unwrap_or(&empty);
                    match self.call(name, args) {
                        Ok(text) => json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text", "text": text}], "isError": false}
                        }),
                        Err(e) => json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": {"content": [{"type": "text", "text": e.to_string()}], "isError": true}
                        }),
                    }
                }
                other => {
                    if id.is_none() {
                        continue;
                    }
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32601, "message": format!("method not found: {other}")}
                    })
                }
            };
            self.respond(out, response)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn exchange(input: &str, g: Gungnir) -> Vec<Value> {
        let mut server = Server::new(g);
        exchange_on(input, &mut server)
    }

    fn exchange_on(input: &str, server: &mut Server) -> Vec<Value> {
        let mut out: Vec<u8> = Vec::new();
        server
            .serve(BufReader::new(input.as_bytes()), &mut out)
            .unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn initialize_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(msgs[0]["result"]["serverInfo"]["name"], "gungnir");
    }

    #[test]
    fn full_tool_workflow_over_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"start_session","arguments":{"agent":"builder","task":"fix slow checkout"}}}"#,
            "\n",
        );
        let msgs = exchange(input, Gungnir::open(dir.path()).unwrap());
        assert_eq!(msgs.len(), 3); // notification gets no response

        let sid = msgs[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .trim_start_matches("session_id: ")
            .to_string();

        // Session survives in server state across separate serve calls only
        // within one Server; drive the rest through one longer exchange.
        let mut server = Server::new(Gungnir::open(dir.path()).unwrap());
        let mut out: Vec<u8> = Vec::new();
        let input = format!(
            concat!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"start_session","arguments":{{"agent":"builder","task":"fix slow checkout"}}}}}}"#,
                "\n",
                r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"add_observation","arguments":{{"session_id":"{sid}","agent":"builder","text":"seq scan on orders"}}}}}}"#,
                "\n",
                r#"{{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{{"name":"add_attempt","arguments":{{"session_id":"{sid}","agent":"builder","text":"rewrote query","succeeded":true}}}}}}"#,
                "\n",
                r#"{{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{{"name":"end_session","arguments":{{"session_id":"{sid}","agent":"builder","summary":"rewrote checkout"}}}}}}"#,
                "\n",
                r#"{{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{{"name":"recall","arguments":{{"query":"checkout rewrite","layer":"journal","agent":"builder"}}}}}}"#,
                "\n"
            ),
            sid = sid
        );
        server
            .serve(std::io::BufReader::new(input.as_bytes()), &mut out)
            .unwrap();
        let msgs: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(msgs.len(), 5);
        for m in &msgs[..4] {
            assert_eq!(m["result"]["isError"], false, "{}", m);
        }
        let recall_text = msgs[4]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(recall_text.contains("rewrote checkout"), "{recall_text}");
    }

    #[test]
    fn unknown_method_is_protocol_error() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":9,"method":"bogus"}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["error"]["code"], -32601);
    }

    #[test]
    fn tool_error_is_result_with_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"get","arguments":{"id":"nope"}}}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
    }

    #[test]
    fn promote_rejects_bad_id_and_missing_summary() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gungnir::open(dir.path()).unwrap();
        let msgs = exchange(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"promote","arguments":{"from":"nope","agent":"a","summary":"x"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"promote","arguments":{"from":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"a"}}}"#,
                "\n"
            ),
            g,
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("bad id"));
        assert_eq!(msgs[1]["result"]["isError"], true);
        assert!(msgs[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("summary required"));
    }

    #[test]
    fn list_journal_requires_agent_and_unknown_layer_errors() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gungnir::open(dir.path()).unwrap();
        let msgs = exchange(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list","arguments":{"layer":"journal"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list","arguments":{"layer":"scratch"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"recall","arguments":{"query":"x","layer":"nope"}}}"#,
                "\n"
            ),
            g,
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("agent required"));
        assert_eq!(msgs[1]["result"]["isError"], true);
        assert!(msgs[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown layer"));
        assert_eq!(msgs[2]["result"]["isError"], true);
        assert!(msgs[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown layer"));
    }

    #[test]
    fn recall_journal_requires_agent() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"query":"x","layer":"journal"}}}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("agent required"));
    }

    #[test]
    fn supersede_and_rollback_reject_malformed_ids() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"supersede","arguments":{"id":"nope","agent":"a","summary":"x"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"rollback","arguments":{"id":"nope","agent":"a"}}}"#,
                "\n"
            ),
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("bad id"));
        assert_eq!(msgs[1]["result"]["isError"], true);
        assert!(msgs[1]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("bad id"));
    }

    #[test]
    fn supersede_unknown_id_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"supersede","arguments":{"id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","agent":"a","summary":"x"}}}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("not found"));
    }

    /// semantic:true must fail loudly on a default build — keyword results
    /// in its place would be a silent degrade.
    #[cfg(not(feature = "embed"))]
    #[test]
    fn semantic_true_errors_without_the_embed_feature() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"query":"postgres","semantic":true}}}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("embed"));
    }

    /// A present-but-non-boolean `semantic` errors rather than reading as
    /// false — `as_bool().unwrap_or(false)` would silently degrade to
    /// keyword and answer `no matches`.
    #[test]
    fn non_boolean_semantic_errors() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"query":"postgres","semantic":"yes"}}}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("boolean"));
    }

    /// semantic:true must actually reach hybrid search. The uniform
    /// embedder scores every pair cosine 1.0, so the target is reachable
    /// through the vector channel alone — no shared keyword token — and a
    /// keyword answer can only mean the flag degraded silently.
    #[cfg(feature = "embed")]
    #[test]
    fn semantic_true_dispatches_to_hybrid_search() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gungnir::open(dir.path()).unwrap();
        let codex = g.codex().unwrap();
        let mut e = crate::entry::Entry::new("ops", EntryKind::Decision, "postgres pooling limits");
        e.verify("ops", None);
        codex.create(&e).unwrap();

        let mut server = Server::new(g);
        server.embedder = Some(Box::new(crate::embedding::Uniform));
        let msgs = exchange_on(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recall","arguments":{"query":"query cache eviction","semantic":true}}}"#,
            &mut server,
        );
        let text = msgs[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("postgres pooling limits"), "{text}");
    }

    /// The brief arm shares `semantic_enabled`, so a mistyped value must
    /// error there too — not fall through to a keyword briefing.
    #[test]
    fn brief_non_boolean_semantic_errors() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"brief","arguments":{"agent":"ops","task":"cache eviction","semantic":"yes"}}}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("boolean"));
    }

    /// brief with semantic:true errors on a default build, same as recall.
    #[cfg(not(feature = "embed"))]
    #[test]
    fn brief_semantic_errors_without_the_embed_feature() {
        let dir = tempfile::tempdir().unwrap();
        let msgs = exchange(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"brief","arguments":{"agent":"ops","task":"cache eviction","semantic":true}}}"#,
            Gungnir::open(dir.path()).unwrap(),
        );
        assert_eq!(msgs[0]["result"]["isError"], true);
        assert!(msgs[0]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("embed"));
    }

    /// brief with semantic:true must reach hybrid search — a keyword
    /// briefing in its place is the silent degrade the flag exists to
    /// prevent.
    #[cfg(feature = "embed")]
    #[test]
    fn brief_semantic_dispatches_to_hybrid_search() {
        let dir = tempfile::tempdir().unwrap();
        let g = Gungnir::open(dir.path()).unwrap();
        let codex = g.codex().unwrap();
        let mut e = crate::entry::Entry::new("ops", EntryKind::Decision, "postgres pooling limits");
        e.verify("ops", None);
        codex.create(&e).unwrap();

        let mut server = Server::new(g);
        server.embedder = Some(Box::new(crate::embedding::Uniform));
        let msgs = exchange_on(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"brief","arguments":{"agent":"ops","task":"query cache eviction","semantic":true}}}"#,
            &mut server,
        );
        let text = msgs[0]["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("postgres pooling limits"), "{text}");
    }
}
