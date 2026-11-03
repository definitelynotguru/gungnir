//! gungnir — local-first, markdown-native memory for AI agents.
//!
//! Three layers:
//! - **Scratch**: per-task working memory (ephemeral)
//! - **Journal**: per-agent private history of attempts
//! - **Codex**: shared, topic-organized source of truth
//!
//! Before each task, a [`briefing`] compiles what's current, superseded,
//! and previously failed into a short context payload.

pub mod briefing;
pub mod cli;
pub mod conflicts;
pub mod embedding;
pub mod entry;
pub mod error;
pub mod gungnir;
pub mod id;
pub mod layout;
pub mod mcp;
pub mod recall;
pub mod rollback;
pub mod store;
pub mod validate;

pub use briefing::{Briefing, BriefingInput};
pub use conflicts::{ConflictId, ConflictRecord, ConflictSignal, ConflictStatus, ConflictVerb};
pub use entry::{
    Entry, EntryKind, Evidence, RecordStatus, Status, VerificationRecord, VerificationState,
};
pub use error::{Error, Result};
pub use gungnir::{
    EvidenceCheck, Gungnir, Layer, PeerCoverage, Promotion, RevalidateReport, Session, StatsReport,
    WriteOutcome,
};
pub use id::EntryId;
pub use recall::{Coverage, Hit, Query, SearchOutcome};
pub use store::{Scan, Skipped, Store};
