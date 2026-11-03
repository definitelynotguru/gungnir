//! The audit surface: `git log`/`diff`/`blame`/`review` for belief.
//!
//! Everything here is a pure read over already-loaded entries — the CLI
//! decides how to print, and the store stays canonical. Two pieces of
//! domain knowledge live in this module: what counts as *superseded* at an
//! instant (mirroring the current view's rule), and what an audit event is
//! (entry creation plus each verification-log transition).

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::entry::{Entry, EntryKind, Evidence, VerificationState};
use crate::id::EntryId;
use crate::recall::{self, CorpusStats, StateAt};
use crate::{Error, Result};

/// One entry plus the partition it was found in: `codex`,
/// `journal/<agent>`, or `scratch/<session>`.
#[derive(Clone, Debug)]
pub struct Located {
    pub partition: String,
    pub entry: Entry,
}

/// Whether `partition` ("journal/builder") sits inside `layer` ("journal").
pub fn in_layer(partition: &str, layer: &str) -> bool {
    partition == layer || partition.starts_with(&format!("{layer}/"))
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Collapse a stored text field to one display line: control characters
/// (newlines included) become spaces so a crafted value can't forge extra
/// rows in audit output.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn fmt_state(e: &Entry) -> String {
    match &e.verification {
        VerificationState::Unverified => "unverified".into(),
        VerificationState::Verified => "verified".into(),
        VerificationState::Contradicted { by } => format!("contradicted-by {by}"),
        VerificationState::RolledBack => "rolled-back".into(),
    }
}

// -- log <id> ---------------------------------------------------------------

/// One belief's verification log and revises lineage.
#[derive(Clone, Debug)]
pub struct EntryReport {
    pub partition: String,
    pub entry: Entry,
    /// Ancestors oldest-first (the chain root last in print order).
    pub ancestors: Vec<Located>,
    /// The full descendant chain: every entry reachable through `revises`,
    /// oldest-first, including rollback records and rolled-back revisions.
    pub chain: Vec<Located>,
    /// The live head that replaced this entry, by current-recall rules —
    /// rollback records and rolled-back revisions do not replace.
    pub replacement: Option<Located>,
    /// Entries anywhere that cite this one as evidence.
    pub cited_by: Vec<Located>,
    /// Entries anywhere contradicted *by this one*.
    pub contradicts: Vec<Located>,
}

fn find(all: &[Located], id: EntryId) -> Option<&Located> {
    all.iter().find(|l| l.entry.id == id)
}

/// Assemble the report `log <id>` and `blame` print. `all` is the whole
/// store scan so cross-layer citations and chain links resolve.
pub fn entry_report(id: EntryId, all: &[Located]) -> Option<EntryReport> {
    let located = find(all, id)?;
    let by_id: HashMap<EntryId, &Located> = all.iter().map(|l| (l.entry.id, l)).collect();

    let mut ancestors = Vec::new();
    let mut visited = HashSet::from([id]);
    let mut cur = located.entry.revises;
    while let Some(prev) = cur {
        if !visited.insert(prev) {
            break; // revises cycle: degrade, don't abort an audit read
        }
        let Some(l) = by_id.get(&prev) else { break };
        ancestors.push((*l).clone());
        cur = l.entry.revises;
    }
    ancestors.reverse();

    // The complete descendant lineage, walked transitively so `log` shows
    // every generation, not just direct children.
    let mut chain: Vec<Located> = Vec::new();
    let mut seen: HashSet<EntryId> = HashSet::from([id]);
    let mut frontier: Vec<Located> = all
        .iter()
        .filter(|l| l.entry.revises == Some(id))
        .cloned()
        .collect();
    while !frontier.is_empty() {
        let mut next_frontier = Vec::new();
        for l in frontier {
            if !seen.insert(l.entry.id) {
                continue; // revises cycle: degrade, don't abort an audit read
            }
            next_frontier.extend(
                all.iter()
                    .filter(|c| c.entry.revises == Some(l.entry.id))
                    .cloned(),
            );
            chain.push(l);
        }
        frontier = next_frontier;
    }
    chain.sort_by_key(|l| l.entry.timestamp);

    // `replaced by` applies the same live-revision rule as current recall:
    // a rollback record restores, it does not replace, and a rolled-back
    // revision cannot be the current head either.
    // The live head that replaced this entry: follow live successor edges
    // from the queried id to the last reachable revision, with a visited
    // guard for malformed cycles. `superseded_ids` already excludes
    // rolled-back edges and rollback records, so the walk stops where
    // current recall would reveal the tail again.
    let superseded = superseded_ids(all, None);
    let mut replacement = None;
    let mut guard: HashSet<EntryId> = HashSet::from([id]);
    let mut cur = id;
    while let Some(&next) = superseded.get(&cur) {
        if !guard.insert(next) {
            break;
        }
        let Some(l) = find(all, next) else { break };
        replacement = Some(l.clone());
        cur = next;
    }

    let cited_by: Vec<Located> = all
        .iter()
        .filter(|l| {
            l.entry
                .evidence
                .iter()
                .any(|ev| matches!(ev, Evidence::Ref { id: rid } if *rid == id))
                || l.entry.wikilink_ids().contains(&id)
        })
        .cloned()
        .collect();

    let contradicts: Vec<Located> = all
        .iter()
        .filter(
            |l| matches!(l.entry.verification, VerificationState::Contradicted { by } if by == id),
        )
        .cloned()
        .collect();

    Some(EntryReport {
        partition: located.partition.clone(),
        entry: located.entry.clone(),
        ancestors,
        chain,
        replacement,
        cited_by,
        contradicts,
    })
}

/// `log <id>` text: entry header, revises chain, verification records.
pub fn format_report(r: &EntryReport, since: Option<DateTime<Utc>>) -> String {
    let mut out = format!(
        "{}  {}  {}  {}  [{}]",
        r.entry.id,
        r.partition,
        fmt_time(r.entry.timestamp),
        one_line(&r.entry.agent),
        fmt_state(&r.entry),
    );
    out.push_str(&format!(
        "\n  {}  {}",
        r.entry.kind,
        one_line(&r.entry.summary)
    ));
    if let Some(prev) = r.entry.revises {
        out.push_str(&format!("\n  revises {prev}"));
    }

    out.push_str("\n\nlineage\n");
    for a in &r.ancestors {
        out.push_str(&format!(
            "  {}  {}  {}\n",
            a.entry.id,
            fmt_state(&a.entry),
            one_line(&a.entry.summary)
        ));
    }
    out.push_str(&format!(
        "  {}  {}  {}  <- this entry\n",
        r.entry.id,
        fmt_state(&r.entry),
        one_line(&r.entry.summary)
    ));
    for s in &r.chain {
        out.push_str(&format!(
            "  {}  {}  {}\n",
            s.entry.id,
            fmt_state(&s.entry),
            one_line(&s.entry.summary)
        ));
    }

    let records: Vec<_> = r
        .entry
        .verification_log
        .iter()
        .filter(|rec| since.is_none_or(|t| rec.timestamp >= t))
        .collect();
    out.push_str("\nverification log\n");
    if records.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for rec in records {
            out.push_str(&format!(
                "  {}  {} by {}",
                fmt_time(rec.timestamp),
                rec.status,
                one_line(&rec.verifier)
            ));
            if let Some(note) = &rec.note {
                out.push_str(&format!(" — {}", one_line(note)));
            }
            out.push('\n');
        }
    }

    let mentions = r.entry.entity_mentions();
    if !mentions.is_empty() {
        out.push_str(&format!("\nentities  {}\n", mentions.join(", ")));
    }
    let links = r.entry.wikilink_ids();
    if !links.is_empty() {
        out.push_str(&format!(
            "wikilinks {}\n",
            links
                .iter()
                .map(EntryId::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !r.cited_by.is_empty() {
        out.push_str("\ncited by\n");
        for l in &r.cited_by {
            out.push_str(&format!(
                "  {}  {}  {}\n",
                l.entry.id,
                l.partition,
                one_line(&l.entry.summary)
            ));
        }
    }
    if !r.contradicts.is_empty() {
        out.push_str("\nthis entry contradicts\n");
        for l in &r.contradicts {
            out.push_str(&format!(
                "  {}  {}  {}\n",
                l.entry.id,
                l.partition,
                one_line(&l.entry.summary)
            ));
        }
    }
    out
}

// -- log --since --------------------------------------------------------------

/// An audit-log event kind: entry creation plus the three verification
/// transitions the log records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Created,
    Verified,
    Contradicted,
    RolledBack,
}

#[derive(Clone, Debug)]
pub struct AuditEvent {
    pub when: DateTime<Utc>,
    pub kind: EventKind,
    /// `entry.agent` for creation, `record.verifier` for transitions.
    pub actor: String,
    pub partition: String,
    pub id: EntryId,
    pub summary: String,
    pub note: Option<String>,
}

/// Chronological scan across partitions. `since` filters by event time;
/// `agent` matches the event actor — the entry's agent for creation, the
/// verifier for transitions.
pub fn scan_events(
    all: &[Located],
    since: Option<DateTime<Utc>>,
    agent: Option<&str>,
) -> Vec<AuditEvent> {
    let mut events = Vec::new();
    let mut push = |ev: AuditEvent| {
        if since.is_some_and(|t| ev.when < t) {
            return;
        }
        if let Some(a) = agent {
            if ev.actor != a {
                return;
            }
        }
        events.push(ev);
    };

    for l in all {
        push(AuditEvent {
            when: l.entry.timestamp,
            kind: EventKind::Created,
            actor: one_line(&l.entry.agent),
            partition: l.partition.clone(),
            id: l.entry.id,
            summary: one_line(&l.entry.summary),
            note: None,
        });
        for rec in &l.entry.verification_log {
            let kind = match rec.status {
                crate::entry::RecordStatus::Verified => EventKind::Verified,
                crate::entry::RecordStatus::Contradicted => EventKind::Contradicted,
                crate::entry::RecordStatus::RolledBack => EventKind::RolledBack,
                _ => continue, // unverified records are not audit events we print
            };
            push(AuditEvent {
                when: rec.timestamp,
                kind,
                actor: one_line(&rec.verifier),
                partition: l.partition.clone(),
                id: l.entry.id,
                summary: one_line(&l.entry.summary),
                note: rec.note.as_deref().map(one_line),
            });
        }
    }
    events.sort_by(|a, b| a.when.cmp(&b.when).then(a.id.cmp(&b.id)));
    events
}

fn event_label(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Created => "created",
        EventKind::Verified => "verified",
        EventKind::Contradicted => "contradicted",
        EventKind::RolledBack => "rolled_back",
    }
}

pub fn format_events(events: &[AuditEvent]) -> String {
    if events.is_empty() {
        return "(no events)\n".into();
    }
    let mut out = String::new();
    for ev in events {
        out.push_str(&format!(
            "{}  {:<11}  {:<20}  {:<12}  {}  {}",
            fmt_time(ev.when),
            event_label(ev.kind),
            ev.partition,
            ev.actor,
            ev.id,
            ev.summary,
        ));
        if let Some(note) = &ev.note {
            out.push_str(&format!("  ({note})"));
        }
        out.push('\n');
    }
    out
}

// -- diff <t1> <t2> -----------------------------------------------------------

/// An entry as it stood at the snapshot instant.
#[derive(Clone, Debug)]
pub struct SnapshotEntry {
    pub located: Located,
    pub state: StateAt,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub at: DateTime<Utc>,
    pub entries: Vec<SnapshotEntry>,
    /// Superseded id → the successor that hides it, evaluated at `at`.
    pub superseded: HashMap<EntryId, EntryId>,
}

/// Ids hidden by a live, non-rollback successor at `as_of` (None = now).
/// Same rule `current_only` applies: a rolled-back head reveals its tail,
/// and a rollback record's revises pointer restores, not replaces.
fn superseded_ids(entries: &[Located], as_of: Option<DateTime<Utc>>) -> HashMap<EntryId, EntryId> {
    let mut latest: HashMap<EntryId, Entry> = HashMap::new();
    for l in entries {
        if let Some(t) = as_of {
            if l.entry.timestamp > t {
                continue;
            }
        }
        if l.entry.kind == EntryKind::Rollback
            || recall::state_at(&l.entry, as_of) == StateAt::RolledBack
        {
            continue;
        }
        if let Some(prev) = l.entry.revises {
            let replace = latest
                .get(&prev)
                .is_none_or(|e| e.timestamp < l.entry.timestamp);
            if replace {
                latest.insert(prev, l.entry.clone());
            }
        }
    }
    latest.into_iter().map(|(k, e)| (k, e.id)).collect()
}

/// What we believed at `at`, optionally restricted to a topic. Topic
/// matching reuses the recall scorer over a corpus built from the
/// snapshot's own pool.
pub fn snapshot(all: &[Located], at: DateTime<Utc>, topic: Option<&str>) -> Snapshot {
    let present: Vec<&Located> = all.iter().filter(|l| l.entry.timestamp <= at).collect();
    let superseded = superseded_ids(
        &present.iter().map(|l| (*l).clone()).collect::<Vec<_>>(),
        Some(at),
    );

    let entries: Vec<SnapshotEntry> = match topic {
        None => present
            .into_iter()
            .map(|l| SnapshotEntry {
                located: l.clone(),
                state: recall::state_at(&l.entry, Some(at)),
            })
            .collect(),
        Some(topic) => {
            let mut qtokens = recall::tokenize(topic);
            qtokens.sort();
            qtokens.dedup();
            let stats = CorpusStats::new(present.iter().map(|l| &l.entry));
            present
                .into_iter()
                .filter(|l| recall::score(&l.entry, &qtokens, &stats) > 0.0)
                .map(|l| SnapshotEntry {
                    located: l.clone(),
                    state: recall::state_at(&l.entry, Some(at)),
                })
                .collect()
        }
    };
    Snapshot {
        at,
        entries,
        superseded,
    }
}

#[derive(Clone, Debug)]
pub struct Superseded {
    pub entry: SnapshotEntry,
    /// The revision that hid it, when resolvable inside the snapshot.
    pub by: Option<EntryId>,
}

#[derive(Clone, Debug, Default)]
pub struct SnapshotDiff {
    /// Created between the instants.
    pub added: Vec<SnapshotEntry>,
    /// Reached `verified` between the instants.
    pub verified: Vec<SnapshotEntry>,
    /// Visible at t1, hidden by a successor at t2.
    pub superseded: Vec<Superseded>,
    /// Became `contradicted` between the instants.
    pub contradicted: Vec<SnapshotEntry>,
    /// Became `rolled_back` between the instants.
    pub rolled_back: Vec<SnapshotEntry>,
}

/// Compare two snapshots over the same store scan.
pub fn snapshot_diff(from: &Snapshot, to: &Snapshot) -> SnapshotDiff {
    let at1: HashMap<EntryId, &SnapshotEntry> = from
        .entries
        .iter()
        .map(|e| (e.located.entry.id, e))
        .collect();
    let mut diff = SnapshotDiff::default();

    for e in &to.entries {
        let id = e.located.entry.id;
        let Some(before) = at1.get(&id) else {
            diff.added.push(e.clone());
            continue;
        };
        let newly_superseded =
            !from.superseded.contains_key(&id) && to.superseded.contains_key(&id);
        if newly_superseded {
            diff.superseded.push(Superseded {
                entry: e.clone(),
                by: to.superseded.get(&id).copied(),
            });
        }
        if before.state != StateAt::Verified && e.state == StateAt::Verified {
            diff.verified.push(e.clone());
        }
        if before.state != StateAt::Contradicted && e.state == StateAt::Contradicted {
            diff.contradicted.push(e.clone());
        }
        if before.state != StateAt::RolledBack && e.state == StateAt::RolledBack {
            diff.rolled_back.push(e.clone());
        }
    }
    diff
}

fn entry_line(prefix: &str, e: &SnapshotEntry) -> String {
    format!(
        "{prefix}  {}  {:<20}  {:<24}  {}",
        e.located.entry.id,
        e.located.partition,
        e.located.entry.kind.to_string(),
        one_line(&e.located.entry.summary)
    )
}

pub fn format_diff(
    diff: &SnapshotDiff,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    topic: Option<&str>,
) -> String {
    let mut out = format!("diff  {}  →  {}", fmt_time(from), fmt_time(to));
    if let Some(t) = topic {
        out.push_str(&format!("  (topic: {t})"));
    }
    out.push('\n');

    let section = |out: &mut String, title: &str, n: usize| {
        if n > 0 {
            out.push_str(&format!("\n{title} ({n})\n"));
        }
    };

    section(&mut out, "added", diff.added.len());
    for e in &diff.added {
        out.push_str(&entry_line("+", e));
        out.push('\n');
    }
    section(&mut out, "verified", diff.verified.len());
    for e in &diff.verified {
        out.push_str(&entry_line("✓", e));
        out.push('\n');
    }
    section(&mut out, "superseded", diff.superseded.len());
    for s in &diff.superseded {
        out.push_str(&entry_line("~", &s.entry));
        if let Some(by) = s.by {
            out.push_str(&format!("  by {by}"));
        }
        out.push('\n');
    }
    section(&mut out, "contradicted", diff.contradicted.len());
    for e in &diff.contradicted {
        out.push_str(&entry_line("!", e));
        out.push('\n');
    }
    section(&mut out, "rolled back", diff.rolled_back.len());
    for e in &diff.rolled_back {
        out.push_str(&entry_line("-", e));
        out.push('\n');
    }

    if diff.added.is_empty()
        && diff.verified.is_empty()
        && diff.superseded.is_empty()
        && diff.contradicted.is_empty()
        && diff.rolled_back.is_empty()
    {
        out.push_str("\n(no changes)\n");
    }
    out
}

// -- blame <id-or-topic> ------------------------------------------------------

/// Resolve a blame target: an exact id, else the best topic match across
/// the whole store (BM25-lite over summary + body).
pub fn resolve(all: &[Located], target: &str) -> Result<Located> {
    if let Ok(id) = target.parse::<EntryId>() {
        return find(all, id).cloned().ok_or(Error::NotFound(id));
    }
    let mut qtokens = recall::tokenize(target);
    qtokens.sort();
    qtokens.dedup();
    if qtokens.is_empty() {
        return Err(Error::Invalid(format!("blame cannot resolve '{target}'")));
    }
    let stats = CorpusStats::new(all.iter().map(|l| &l.entry));
    all.iter()
        .filter_map(|l| {
            let s = recall::score(&l.entry, &qtokens, &stats);
            (s > 0.0).then_some((s, l))
        })
        .max_by(|a, b| a.0.total_cmp(&b.0).then(a.1.entry.id.cmp(&b.1.entry.id)))
        .map(|(_, l)| l.clone())
        .ok_or_else(|| Error::Invalid(format!("no belief matches '{target}'")))
}

/// `blame` text: who vouched, what it revised, what replaced it.
pub fn format_blame(r: &EntryReport) -> String {
    let e = &r.entry;
    let mut out = format!(
        "{}  \"{}\"  ({})\nbelief state: {}\n",
        e.id,
        one_line(&e.summary),
        r.partition,
        fmt_state(e)
    );

    out.push_str("vouched by:");
    if e.verification_log.is_empty() {
        out.push_str(" nobody yet");
    } else {
        for rec in &e.verification_log {
            out.push_str(&format!(
                "\n  {}  {} by {}",
                fmt_time(rec.timestamp),
                rec.status,
                one_line(&rec.verifier)
            ));
            if let Some(note) = &rec.note {
                out.push_str(&format!(" — {}", one_line(note)));
            }
        }
    }
    out.push('\n');

    match (e.revises, r.ancestors.last()) {
        (Some(prev), Some(l)) => out.push_str(&format!(
            "supersedes: {}  \"{}\"\n",
            prev,
            one_line(&l.entry.summary)
        )),
        (Some(prev), None) => out.push_str(&format!("supersedes: {prev}\n")),
        (None, _) => {}
    }

    match &r.replacement {
        Some(l) => out.push_str(&format!(
            "replaced by: {}  \"{}\"  [{}]\n",
            l.entry.id,
            one_line(&l.entry.summary),
            fmt_state(&l.entry)
        )),
        None if matches!(e.verification, VerificationState::RolledBack) => {
            out.push_str("replaced by: nothing — retracted\n")
        }
        None => out.push_str("replaced by: nothing — still current\n"),
    }

    if !r.cited_by.is_empty() {
        out.push_str("cited by:");
        for l in &r.cited_by {
            out.push_str(&format!(
                "\n  {}  {}  {}",
                l.entry.id,
                l.partition,
                one_line(&l.entry.summary)
            ));
        }
        out.push('\n');
    }
    if !r.contradicts.is_empty() {
        out.push_str("this belief contradicts:");
        for l in &r.contradicts {
            out.push_str(&format!(
                "\n  {}  {}",
                l.entry.id,
                one_line(&l.entry.summary)
            ));
        }
        out.push('\n');
    }

    let mentions = e.entity_mentions();
    if !mentions.is_empty() {
        out.push_str(&format!("entities: {}\n", mentions.join(", ")));
    }
    out
}

// -- review -------------------------------------------------------------------

/// One entry awaiting human judgment.
#[derive(Clone, Debug)]
pub struct ReviewItem {
    pub partition: String,
    /// `project_id` as the topic grouping; "(no project)" when unset.
    pub project: String,
    pub entry: Entry,
    /// Machine-originated (`source_tool` set) — later extraction.
    pub extracted: bool,
}

/// Belief-kind entries still `unverified`, excluding chain tails a newer
/// live revision already replaced.
pub fn review_queue(all: &[Located]) -> Vec<ReviewItem> {
    let superseded = superseded_ids(all, None);
    all.iter()
        .filter(|l| {
            l.entry.verification == VerificationState::Unverified
                && !superseded.contains_key(&l.entry.id)
                && matches!(
                    l.entry.kind,
                    EntryKind::Decision | EntryKind::Observation | EntryKind::Attempt
                )
        })
        .map(|l| ReviewItem {
            partition: l.partition.clone(),
            project: l
                .entry
                .project_id
                .clone()
                .map(|p| one_line(&p))
                .unwrap_or_else(|| "(no project)".into()),
            extracted: l.entry.source_tool.is_some(),
            entry: l.entry.clone(),
        })
        .collect()
}

pub fn format_review(items: &[ReviewItem]) -> String {
    if items.is_empty() {
        return "(nothing awaiting review)\n".into();
    }
    let mut out = format!("{} entr", items.len());
    out.push_str(if items.len() == 1 { "y" } else { "ies" });
    out.push_str(" awaiting review\n");

    // partition → project → items, sorted; insertion order keeps entries
    // chronological inside a group.
    let mut tree: BTreeMap<String, BTreeMap<String, Vec<&ReviewItem>>> = BTreeMap::new();
    for item in items {
        tree.entry(item.partition.clone())
            .or_default()
            .entry(item.project.clone())
            .or_default()
            .push(item);
    }
    for (partition, projects) in tree {
        out.push_str(&format!("\n{partition}\n"));
        for (project, group) in projects {
            if project == "(no project)" {
                out.push_str("  (no project)\n");
            } else {
                out.push_str(&format!("  [{project}]\n"));
            }
            for item in group {
                let extracted = if item.extracted { "  [extracted]" } else { "" };
                out.push_str(&format!(
                    "    {}  {:<12}  {:<11}  {}{}\n",
                    item.entry.id,
                    one_line(&item.entry.agent),
                    item.entry.kind.to_string(),
                    one_line(&item.entry.summary),
                    extracted,
                ));
                out.push_str(&format!(
                    "        act: gungnir verify {0} | gungnir supersede {0} --agent me --summary \"…\" | gungnir contradict {0} --by <id>\n",
                    item.entry.id
                ));
            }
        }
    }
    out
}

// -- entities -----------------------------------------------------------------

/// `gungnir entities` text: every entity name with its entry count, or the
/// entries under one name.
pub fn format_entities(all: &[Located], name: Option<&str>) -> String {
    match name {
        Some(name) => {
            let matches: Vec<&Located> = all
                .iter()
                .filter(|l| l.entry.entity_matches(name))
                .collect();
            if matches.is_empty() {
                return format!("(no entries mention entity '{name}')\n");
            }
            let mut out = format!("entity '{name}' — {} entr", matches.len());
            out.push_str(if matches.len() == 1 { "y\n" } else { "ies\n" });
            for l in matches {
                out.push_str(&format!(
                    "  {}  {:<20}  {}  {}\n",
                    l.entry.id,
                    l.partition,
                    fmt_state(&l.entry),
                    one_line(&l.entry.summary)
                ));
            }
            out
        }
        None => {
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for l in all {
                for m in l.entry.entity_mentions() {
                    *counts.entry(m).or_default() += 1;
                }
            }
            if counts.is_empty() {
                return "(no entities)\n".into();
            }
            let mut out = String::new();
            for (name, n) in counts {
                out.push_str(&format!("{name}\t{n}\n"));
            }
            out
        }
    }
}
