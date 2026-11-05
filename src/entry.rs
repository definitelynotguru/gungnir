//! The entry model: one markdown file, YAML frontmatter + free-form body.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::id::EntryId;

/// What kind of knowledge this entry captures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Decision,
    Observation,
    Attempt,
    Review,
    Rollback,
    /// Archive record of one completed task session, filed in the Journal.
    Session,
}

impl std::fmt::Display for EntryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Decision => "decision",
            Self::Observation => "observation",
            Self::Attempt => "attempt",
            Self::Review => "review",
            Self::Rollback => "rollback",
            Self::Session => "session",
        };
        f.write_str(s)
    }
}

/// Lifecycle status. Open entries must name an owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Open { assigned_to: String },
    Closed,
}

/// Verification state machine.
///
/// Entries are born [`VerificationState::Unverified`]; `verified` is only
/// reachable through [`Entry::verify`]. `contradicted` must name the entry
/// that contradicts it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verification", rename_all = "snake_case")]
pub enum VerificationState {
    Unverified,
    Verified,
    Contradicted {
        #[serde(rename = "contradicted_by")]
        by: EntryId,
    },
    RolledBack,
}

/// One link in the evidence chain: either a filesystem artifact or another
/// entry in the store.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Evidence {
    File {
        path: String,
        excerpt: String,
        sha256: String,
    },
    Ref {
        id: EntryId,
    },
}

impl std::str::FromStr for EntryKind {
    type Err = crate::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "decision" => Ok(Self::Decision),
            "observation" => Ok(Self::Observation),
            "attempt" => Ok(Self::Attempt),
            "review" => Ok(Self::Review),
            "rollback" => Ok(Self::Rollback),
            "session" => Ok(Self::Session),
            other => Err(crate::Error::Invalid(format!("unknown kind '{other}'"))),
        }
    }
}

/// The status a [`VerificationRecord`] asserts. Typed so a misspelled log
/// entry fails to parse instead of silently downgrading a verified fact.
/// Serializes to the same snake_case strings the log has always used.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Unverified,
    Verified,
    Contradicted,
    RolledBack,
}

impl std::fmt::Display for RecordStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Unverified => "unverified",
            Self::Verified => "verified",
            Self::Contradicted => "contradicted",
            Self::RolledBack => "rolled_back",
        };
        f.write_str(s)
    }
}

/// An appended record tracking who verified/contradicted/rolled back, when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationRecord {
    pub verifier: String,
    pub timestamp: DateTime<Utc>,
    pub status: RecordStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A single unit of agent memory.
///
/// Serialization note: `body` is excluded from frontmatter (`skip`) — it is
/// appended as markdown after the closing `---` by the [`crate::store`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub agent: String,
    pub kind: EntryKind,
    /// ≤ 200 chars, enforced at write time.
    pub summary: String,
    pub timestamp: DateTime<Utc>,
    pub status: Status,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    /// The entry this one replaces (supersession chain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revises: Option<EntryId>,
    /// Required for `kind = review`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_of: Option<EntryId>,
    #[serde(flatten)]
    pub verification: VerificationState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification_log: Vec<VerificationRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_tool: Option<String>,
    /// The extraction backend that proposed this entry ("heuristic", a
    /// command name). Set only when `source_tool` is `extract`; audits show
    /// it so machine origin names its machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extracted_by: Option<String>,
    /// Whether an `attempt` entry succeeded. Structured so session archives
    /// and stats don't re-parse a magic prefix out of the body. Older files
    /// carry the outcome as a `outcome: ...` body line instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<bool>,
    /// Names of entities this entry is about. Free-form strings ("checkout",
    /// "orders_archive"), not a graph; body `[[name]]` mentions merge with
    /// these when the entry is scanned. `[[<entry-id>]]` wikilinks are
    /// validated like evidence refs instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,
    #[serde(skip)]
    pub body: String,
}

impl Entry {
    /// Create a new entry. Always born `unverified` — verification is an
    /// explicit, logged transition, never a write-time field.
    pub fn new(agent: impl Into<String>, kind: EntryKind, summary: impl Into<String>) -> Self {
        Self {
            id: EntryId::generate(),
            agent: agent.into(),
            kind,
            summary: summary.into(),
            timestamp: Utc::now(),
            status: Status::Closed,
            project_id: None,
            session_id: None,
            revises: None,
            review_of: None,
            verification: VerificationState::Unverified,
            verification_log: Vec::new(),
            evidence: Vec::new(),
            source_tool: None,
            extracted_by: None,
            outcome: None,
            entities: Vec::new(),
            body: String::new(),
        }
    }

    /// Mark verified, appending a log record. The caller persists via
    /// [`crate::Store::update`].
    pub fn verify(&mut self, verifier: impl Into<String>, note: Option<String>) {
        self.verification = VerificationState::Verified;
        self.verification_log.push(VerificationRecord {
            verifier: verifier.into(),
            timestamp: Utc::now(),
            status: RecordStatus::Verified,
            note,
        });
    }

    /// Mark contradicted by an existing entry, appending a log record.
    pub fn contradict(&mut self, by: EntryId, verifier: impl Into<String>, note: Option<String>) {
        self.verification = VerificationState::Contradicted { by };
        self.verification_log.push(VerificationRecord {
            verifier: verifier.into(),
            timestamp: Utc::now(),
            status: RecordStatus::Contradicted,
            note,
        });
    }

    /// Mark rolled back, appending a log record.
    pub fn mark_rolled_back(&mut self, verifier: impl Into<String>) {
        self.verification = VerificationState::RolledBack;
        self.verification_log.push(VerificationRecord {
            verifier: verifier.into(),
            timestamp: Utc::now(),
            status: RecordStatus::RolledBack,
            note: None,
        });
    }

    /// `[[target]]` links inside the body, in order, without the brackets.
    /// A target may be an entity name or an [`EntryId`]; the caller
    /// distinguishes via parse.
    pub fn wikilink_targets(&self) -> Vec<String> {
        wikilink_targets(&self.body)
    }

    /// Id-shaped `[[target]]` links in the body. These are validated like
    /// evidence refs at write time; name-shaped targets are entity mentions.
    pub fn wikilink_ids(&self) -> Vec<EntryId> {
        self.wikilink_targets()
            .into_iter()
            .filter_map(|t| t.parse::<EntryId>().ok())
            .collect()
    }

    /// Every entity name this entry claims, frontmatter `entities:` merged
    /// with `[[name]]` body mentions, deduped case-insensitively. A body
    /// target only counts when it meets the entity-name rule — a `[[` span
    /// in free text ("see [[1,2],[3,4]]") is prose, not a claim.
    pub fn entity_mentions(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |name: String| {
            let key = name.trim().to_lowercase();
            if !key.is_empty() && !out.iter().any(|n| n.to_lowercase() == key) {
                out.push(key);
            }
        };
        for e in &self.entities {
            push(e.clone());
        }
        for t in self.wikilink_targets() {
            if t.parse::<EntryId>().is_err() && crate::validate::valid_entity_name(&t) {
                push(t);
            }
        }
        out
    }

    /// Whether `name` matches an entity mention, case-insensitively.
    pub fn entity_matches(&self, name: &str) -> bool {
        let key = name.trim().to_lowercase();
        !key.is_empty() && self.entity_mentions().iter().any(|n| n == &key)
    }
}

/// Extract `[[target]]` markers from arbitrary text. A link cannot span
/// lines: an unterminated `[[` is literal text, not a link that swallows
/// everything up to the next `]]` it can find.
fn wikilink_targets(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("[[") {
        let after_open = &rest[open + 2..];
        let line_end = after_open.find('\n').unwrap_or(after_open.len());
        match after_open[..line_end].find("]]") {
            Some(close) => {
                let target = after_open[..close].trim();
                if !target.is_empty() {
                    out.push(target.to_string());
                }
                rest = &after_open[close + 2..];
            }
            // Literal `[[` on this line: skip to the next line so a real
            // link later in the body still registers.
            None => rest = &after_open[line_end..],
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_entries_are_unverified() {
        let e = Entry::new("agent-a", EntryKind::Decision, "chose sqlite");
        assert_eq!(e.verification, VerificationState::Unverified);
    }

    #[test]
    fn verify_appends_log_record() {
        let mut e = Entry::new("agent-a", EntryKind::Decision, "chose sqlite");
        e.verify("human-review", Some("confirmed".into()));
        assert_eq!(e.verification, VerificationState::Verified);
        assert_eq!(e.verification_log.len(), 1);
        assert_eq!(e.verification_log[0].status, RecordStatus::Verified);
    }

    #[test]
    fn log_status_parses_known_spellings_and_rejects_typos() {
        for status in ["unverified", "verified", "contradicted", "rolled_back"] {
            let yaml = format!("verifier: ci\ntimestamp: 2026-01-01T00:00:00Z\nstatus: {status}\n");
            serde_yaml::from_str::<VerificationRecord>(&yaml)
                .unwrap_or_else(|e| panic!("{status} should parse: {e}"));
        }
        let yaml = "verifier: ci\ntimestamp: 2026-01-01T00:00:00Z\nstatus: verrified\n";
        assert!(serde_yaml::from_str::<VerificationRecord>(yaml).is_err());
    }

    #[test]
    fn yaml_roundtrip_preserves_state() {
        let mut e = Entry::new("agent-a", EntryKind::Attempt, "tried index");
        e.project_id = Some("proj".into());
        e.verify("ci", None);

        let yaml = serde_yaml::to_string(&e).unwrap();
        let back: Entry = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, e);
    }
}
