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

use crate::gungnir::Session;
use crate::id::EntryId;
use crate::recall::Query;
use crate::{Error, Gungnir, Result};

const PROTOCOL_VERSION: &str = "2024-11-05";

/// Open sessions by id, so agents only pass a session handle between calls.
pub struct Server {
    g: Gungnir,
    sessions: HashMap<String, Session>,
}

impl Server {
    pub fn new(g: Gungnir) -> Self {
        Self { g, sessions: HashMap::new() }
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
                "description": "Keyword search. layer: codex (default) or journal.",
                "inputSchema": schema(json!({
                    "query": {"type": "string"},
                    "layer": {"type": "string", "enum": ["codex", "journal"]},
                    "agent": {"type": "string"},
                    "limit": {"type": "integer"}
                }), &["query"])
            },
            {
                "name": "brief",
                "description": "Compile the pre-task briefing without opening a session.",
                "inputSchema": schema(json!({
                    "agent": {"type": "string"},
                    "task": {"type": "string"},
                    "limit": {"type": "integer"}
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
                let s = self.g.start_session(agent, task);
                let briefing = self.g.brief(agent, task, 8)?;
                self.sessions.insert(s.id.clone(), s.clone());
                Ok(format!("session_id: {}\n\n{}", s.id, briefing.markdown))
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
                let ok = args.get("succeeded").and_then(Value::as_bool).unwrap_or(false);
                let id = self.g.add_attempt(&s, text, ok)?;
                Ok(id.to_string())
            }
            "end_session" => {
                let s = self.session(args)?;
                let summary = arg("summary").ok_or_else(|| Error::Invalid("summary required".into()))?;
                let report = self.g.end_session(&s, summary, vec![])?;
                self.sessions.remove(&s.id);
                Ok(format!("archived {}", report.journal_id))
            }
            "recall" => {
                let query = arg("query").ok_or_else(|| Error::Invalid("query required".into()))?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
                let hits = match arg("layer") {
                    Some("journal") => {
                        let agent = arg("agent").ok_or_else(|| Error::Invalid("agent required for journal recall".into()))?;
                        self.g.recall_layer(crate::gungnir::Layer::Journal { agent }, &Query::new(query, limit))?
                    }
                    _ => self.g.recall_layer(crate::gungnir::Layer::Codex, &Query::new(query, limit))?,
                };
                if hits.is_empty() {
                    return Ok("no matches".into());
                }
                Ok(hits
                    .iter()
                    .map(|h| format!("{:.3}  {}  {}", h.score, h.entry.id, h.entry.summary))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "brief" => {
                let agent = arg("agent").ok_or_else(|| Error::Invalid("agent required".into()))?;
                let task = arg("task").ok_or_else(|| Error::Invalid("task required".into()))?;
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(8) as usize;
                Ok(self.g.brief(agent, task, limit)?.markdown)
            }
            "verify" => {
                let id: EntryId = arg("id")
                    .ok_or_else(|| Error::Invalid("id required".into()))?
                    .parse()
                    .map_err(|e| Error::Invalid(format!("bad id: {e}")))?;
                let verifier = arg("verifier").unwrap_or("agent");
                self.g.verify(id, verifier, arg("note").map(str::to_owned))?;
                Ok(format!("verified {id}"))
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

    /// Reconstruct the session handle; works across server restarts because
    /// scratch lives on disk keyed by session id.
    fn session(&self, args: &Value) -> Result<Session> {
        let arg = |k: &str| args.get(k).and_then(Value::as_str);
        let id = arg("session_id").ok_or_else(|| Error::Invalid("session_id required".into()))?;
        if let Some(s) = self.sessions.get(id) {
            return Ok(s.clone());
        }
        let agent = arg("agent").ok_or_else(|| {
            Error::Invalid("agent required (unknown session_id)".into())
        })?;
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
                    self.respond(out, json!({
                        "jsonrpc": "2.0", "id": null,
                        "error": {"code": -32700, "message": format!("parse error: {e}")}
                    }))?;
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
                "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": self.tools()}}),
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
}
