//! The store: entries as markdown files under a date-sharded tree.
//!
//! Layout: `<root>/YYYY/MM/DD/<ulid>.md`, plus `<root>/.cache/` for derived
//! data and a `<root>/.lock` file serializing writes across processes.
//!
//! Durability contract:
//! - every write goes to a temp sibling then `rename`s into place (atomic)
//! - multi-entry operations hold an exclusive `flock` on `.lock`
//! - a crash leaves the previous file intact; stale `.tmp-*` siblings are
//!   swept on open

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use fs2::FileExt;

use crate::entry::Entry;
use crate::error::{Error, Result};
use crate::id::EntryId;
use crate::validate;

const TMP_PREFIX: &str = ".tmp-";
/// How long an orphaned temp file must be before the startup sweep removes it.
const TMP_STALE_AFTER: Duration = Duration::from_secs(60);

/// An open store rooted at a directory.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open (creating if needed) the store at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let store = Self { root };
        store.sweep_stale_temps()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path where `id` lives: `YYYY/MM/DD/<id>.md`, sharded by the ULID's
    /// embedded timestamp so the tree is stable regardless of write order.
    pub fn path_for(&self, id: EntryId) -> PathBuf {
        // ULID timestamp is ms since UNIX epoch — decode via datetime.
        let secs = (id.timestamp_ms() / 1000) as i64;
        let nanos = (id.timestamp_ms() % 1000) as u32 * 1_000_000;
        let dt = DateTime::<Utc>::from_timestamp(secs, nanos).unwrap_or_else(Utc::now);
        use chrono::Datelike;
        self.root
            .join(format!("{:04}", dt.year()))
            .join(format!("{:02}", dt.month()))
            .join(format!("{:02}", dt.day()))
            .join(format!("{id}.md"))
    }

    pub fn exists(&self, id: EntryId) -> Result<bool> {
        // Exists iff loadable — guards against torn files counting as present.
        Ok(self.get(id)?.is_some())
    }

    /// Create a new entry after full write-time validation.
    pub fn create(&self, entry: &Entry) -> Result<()> {
        self.create_with(entry, &|id| self.exists(id))
    }

    /// Create with a custom reference resolver, for cross-layer writes
    /// where referenced ids may live in other partitions.
    pub fn create_with(&self, entry: &Entry, exists: validate::Exists<'_>) -> Result<()> {
        if self.exists(entry.id)? {
            return Err(Error::Duplicate(entry.id));
        }
        validate::validate_entry(entry, exists)?;
        let _guard = self.lock()?;
        self.persist_locked(entry)
    }

    /// Overwrite an existing entry (e.g. after `verify`).
    pub fn update(&self, entry: &Entry) -> Result<()> {
        self.update_with(entry, &|id| self.exists(id))
    }

    /// Update with a custom reference resolver; see [`Store::create_with`].
    pub fn update_with(&self, entry: &Entry, exists: validate::Exists<'_>) -> Result<()> {
        if !self.exists(entry.id)? {
            return Err(Error::NotFound(entry.id));
        }
        validate::validate_entry(entry, exists)?;
        let _guard = self.lock()?;
        self.persist_locked(entry)
    }

    /// Load one entry, or `None` if absent.
    pub fn get(&self, id: EntryId) -> Result<Option<Entry>> {
        let path = self.path_for(id);
        if !path.exists() {
            return Ok(None);
        }
        Ok(Some(parse_entry_file(&path)?))
    }

    /// Load one entry or fail with [`Error::NotFound`].
    pub fn require(&self, id: EntryId) -> Result<Entry> {
        self.get(id)?.ok_or(Error::NotFound(id))
    }

    /// All entries, oldest first (ULID order ≈ chronological).
    pub fn entries(&self) -> Result<Vec<Entry>> {
        let mut ids: Vec<EntryId> = Vec::new();
        for dent in walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| e.file_name() != ".cache" && e.file_name() != ".lock")
            .filter_map(|e| e.ok())
        {
            if !dent.file_type().is_file() {
                continue;
            }
            if dent.path().extension() != Some(std::ffi::OsStr::new("md")) {
                continue;
            }
            let stem = dent
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            match stem.parse::<EntryId>() {
                Ok(id) => ids.push(id),
                Err(_) => continue, // not ours; leave foreign files alone
            }
        }
        ids.sort();
        ids.into_iter().map(|id| self.require(id)).collect()
    }

    // -- internals ----------------------------------------------------------

    /// Serialize + atomically place the file. Caller must hold the lock.
    fn persist_locked(&self, entry: &Entry) -> Result<()> {
        let path = self.path_for(entry.id);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut frontmatter = serde_yaml::to_string(entry).map_err(|source| Error::Yaml {
            path: path.clone(),
            source,
        })?;
        // serde_yaml ends with "\n"; normalize to exactly one blank line
        // between frontmatter and body.
        while frontmatter.ends_with("\n\n") {
            frontmatter.pop();
        }
        let file = format!("---\n{frontmatter}---\n\n{}\n", entry.body);

        let tmp = path.with_file_name(format!("{TMP_PREFIX}{}-{}", std::process::id(), entry.id));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(file.as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Exclusive cross-process lock. Held until the guard drops.
    fn lock(&self) -> Result<fs::File> {
        let lock_path = self.root.join(".lock");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    /// Remove orphaned temp files from crashed writes older than
    /// [`TMP_STALE_AFTER`]. Fresh temps may belong to a live writer.
    fn sweep_stale_temps(&self) -> Result<()> {
        let now = std::time::SystemTime::now();
        for dent in walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let name = dent.file_name().to_string_lossy();
            if !name.starts_with(TMP_PREFIX) || !dent.file_type().is_file() {
                continue;
            }
            let stale = fs::metadata(dent.path())
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .is_some_and(|age| age > TMP_STALE_AFTER);
            if stale {
                let _ = fs::remove_file(dent.path());
            }
        }
        Ok(())
    }
}

/// Parse one `---frontmatter---\n\nbody` markdown file.
fn parse_entry_file(path: &Path) -> Result<Entry> {
    let raw = fs::read_to_string(path)?;
    let rest = raw
        .strip_prefix("---\n")
        .ok_or_else(|| Error::Invalid(format!("{}: missing frontmatter opener", path.display())))?;
    let (yaml, body) = rest
        .split_once("\n---\n")
        .ok_or_else(|| Error::Invalid(format!("{}: missing frontmatter closer", path.display())))?;

    let mut entry: Entry = serde_yaml::from_str(yaml).map_err(|source| Error::Yaml {
        path: path.into(),
        source,
    })?;
    entry.body = body.trim_start_matches('\n').trim_end().to_string();
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;

    fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn create_then_get_roundtrips_body_and_fields() {
        let (_dir, store) = tmp_store();
        let mut e = Entry::new("agent-a", EntryKind::Decision, "chose sqlite");
        e.body = "# Why sqlite\n\nZero ops overhead.".into();

        store.create(&e).unwrap();
        let loaded = store.require(e.id).unwrap();
        assert_eq!(loaded, e);
        assert_eq!(loaded.body, "# Why sqlite\n\nZero ops overhead.");
    }

    #[test]
    fn duplicate_create_is_rejected() {
        let (_dir, store) = tmp_store();
        let e = Entry::new("agent-a", EntryKind::Observation, "x");
        store.create(&e).unwrap();
        assert!(matches!(store.create(&e), Err(Error::Duplicate(_))));
    }

    #[test]
    fn files_land_in_day_shards() {
        let (dir, store) = tmp_store();
        let e = Entry::new("a", EntryKind::Observation, "saw it");
        store.create(&e).unwrap();
        let p = store.path_for(e.id);
        assert!(p.exists());
        assert!(p.starts_with(dir.path()));
        let rel = p.strip_prefix(dir.path()).unwrap();
        // YYYY/MM/DD/file.md → 4 components
        assert_eq!(rel.components().count(), 4);
    }

    #[test]
    fn no_temp_files_left_after_successful_write() {
        let (_dir, store) = tmp_store();
        let e = Entry::new("a", EntryKind::Observation, "clean");
        store.create(&e).unwrap();
        let leftovers: Vec<_> = walkdir::WalkDir::new(store.root())
            .into_iter()
            .filter_map(|d| d.ok())
            .filter(|d| d.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn entries_lists_in_chronological_order() {
        let (_dir, store) = tmp_store();
        let a = Entry::new("a", EntryKind::Observation, "first");
        store.create(&a).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
        let b = Entry::new("a", EntryKind::Observation, "second");
        store.create(&b).unwrap();

        let all = store.entries().unwrap();
        let ids: Vec<_> = all.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![a.id, b.id]);
    }
}
