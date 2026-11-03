//! Embedding-based recall: pluggable embedder trait, content-addressed disk
//! cache, and reciprocal rank fusion for hybrid keyword + vector search.
//!
//! Hybrid mode is opt-in per query (CLI `--semantic`, MCP `semantic: true`)
//! and needs a concrete [`Embedder`]. The `embed` cargo feature ships one:
//! bge-small-en-v1.5 through fastembed (ONNX), downloaded on first use into
//! `<root>/.cache`. Without the feature the same surface can take any
//! user-implemented embedder; with no embedder at all, recall falls back to
//! the BM25-lite keyword path — same temporal filters, same coverage signal.
//!
//! The cache is derived state only: `.cache` is safe to delete, never read
//! on the write path, and markdown stays the source of truth.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::recall::{self, CorpusStats, Query, SearchOutcome};
use crate::{Error, Result, Store};

/// Turns text into vectors. Implement this against your embedding provider.
pub trait Embedder: Send + Sync {
    fn model(&self) -> &str;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Every text embeds to the same unit vector, so every pair scores cosine
/// 1.0 — above the vector match floor with zero keyword overlap. Exists so
/// the semantic dispatch surfaces (CLI `--semantic`, MCP `semantic: true`)
/// can be exercised without shipping a model; opt in via
/// `GUNGNIR_TEST_EMBEDDER=1`.
pub struct Uniform;

impl Embedder for Uniform {
    fn model(&self) -> &str {
        "uniform-1"
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![1.0f32]).collect())
    }
}

/// Disk cache in front of any embedder.
///
/// Cache key is `sha256(model + "\n" + normalized_text)` — content-addressed,
/// so editing a file invalidates its vector naturally and switching models
/// partitions the cache by directory. Vectors live under
/// `<root>/.cache/embeddings/<model>/<key>.json`.
pub struct CachedEmbedder<E: Embedder> {
    inner: E,
    cache_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct CachedVector {
    v: Vec<f32>,
}

impl<E: Embedder> CachedEmbedder<E> {
    /// `root` is the gungnir memory root; vectors land under `.cache/` there.
    pub fn new(inner: E, root: impl AsRef<Path>) -> Self {
        let model_dir = sanitize_model(inner.model());
        let cache_dir = root
            .as_ref()
            .join(crate::layout::CACHE)
            .join("embeddings")
            .join(model_dir);
        Self { inner, cache_dir }
    }

    fn cache_path(&self, text: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(self.inner.model());
        hasher.update(b"\n");
        hasher.update(normalize(text).as_bytes());
        let key = hex::encode(hasher.finalize());
        self.cache_dir.join(format!("{key}.json"))
    }
}

impl<E: Embedder> Embedder for CachedEmbedder<E> {
    fn model(&self) -> &str {
        self.inner.model()
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        fs::create_dir_all(&self.cache_dir)?;
        let mut out = Vec::with_capacity(texts.len());
        let mut misses: Vec<(usize, &String)> = Vec::new();
        for (i, t) in texts.iter().enumerate() {
            let path = self.cache_path(t);
            if let Ok(raw) = fs::read(&path) {
                if let Ok(cached) = serde_json::from_slice::<CachedVector>(&raw) {
                    out.push(cached.v);
                    continue;
                }
            }
            misses.push((i, t));
            out.push(Vec::new()); // placeholder, replaced below
        }
        if !misses.is_empty() {
            let fresh: Vec<String> = misses.iter().map(|(_, t)| (*t).clone()).collect();
            let vectors = self.inner.embed(&fresh)?;
            for ((i, t), v) in misses.into_iter().zip(vectors) {
                let path = self.cache_path(t);
                fs::write(
                    &path,
                    serde_json::to_vec(&CachedVector { v: v.clone() })
                        .map_err(|e| Error::Invalid(format!("cache serialize: {e}")))?,
                )?;
                out[i] = v;
            }
        }
        Ok(out)
    }
}

fn sanitize_model(name: &str) -> String {
    crate::layout::sanitize_component(name)
}

/// Lowercase and collapse whitespace so trivial formatting changes don't
/// bust the cache.
fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|y| y * y).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Reciprocal rank fusion over multiple rankings of the same candidate pool.
/// `rankings` hold candidate indexes, best first. k = 60 per the literature.
pub fn rrf_fuse(rankings: &[Vec<usize>], pool_size: usize, k: f64) -> Vec<(usize, f64)> {
    let mut scores = vec![0.0f64; pool_size];
    for ranking in rankings {
        for (rank, &candidate) in ranking.iter().enumerate() {
            scores[candidate] += 1.0 / (k + rank as f64 + 1.0);
        }
    }
    let mut fused: Vec<(usize, f64)> = scores.into_iter().enumerate().collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1));
    fused
}

/// Cosine floor for a vector-only match. Keyword matches need no floor:
/// a token hit is already evidence. This floor keeps corpus-wide noise —
/// every real embedder returns some positive similarity for almost
/// everything — from manufacturing "coverage" the abstention signal would
/// then have to stand behind.
const VECTOR_MATCH_FLOOR: f32 = 0.45;

/// Hybrid search: BM25-lite ranking fused with embedding cosine ranking via
/// RRF, over the same temporal candidate pool and coverage accounting as
/// [`recall::search_with_coverage`]. `as_of` and `current_only` apply
/// identically — there is exactly one temporal filter in the codebase.
///
/// Relevance for the trust×recency formula is the RRF fusion score scaled
/// by 1000 so its magnitude sits near BM25 scores; the ordering comes from
/// the same bucket-first sort either path uses.
///
/// Complexity is O(N) cosine over the pool — fine to ~10k entries, the
/// ceiling documented in docs/DESIGN.md.
pub fn hybrid_search(
    store: &Store,
    query: &Query,
    embedder: &dyn Embedder,
) -> Result<SearchOutcome> {
    let (pool, revised, skipped) = recall::temporal_pool(store, query)?;
    let pool_len = pool.len();
    let stats = CorpusStats::new(recall::stats_pool(&pool, &revised, query).into_iter());

    // Keyword channel. The ranking holds only candidates with signal:
    // zero-score entries have no keyword relevance, and letting them into
    // RRF would make store order influence a vector-only query's results.
    let qtokens = {
        let mut t = recall::tokenize(&query.text);
        t.sort();
        t.dedup();
        t
    };
    let kw_scores: Vec<f64> = pool
        .iter()
        .map(|c| recall::score(&c.entry, &qtokens, &stats))
        .collect();
    let mut kw_rank: Vec<usize> = (0..pool.len()).filter(|&i| kw_scores[i] > 0.0).collect();
    kw_rank.sort_by(|a, b| kw_scores[*b].total_cmp(&kw_scores[*a]));

    // Vector channel. Entries rolled back *now* stay out of the embedder:
    // retracted text must not leave the process through a user-supplied
    // HTTP embedder, nor linger in the vector cache — even on an as_of
    // query that evaluates them back to their pre-retraction state. Their
    // keyword scores still compute locally, so coverage can report them
    // as hidden.
    let embedded: Vec<usize> = pool
        .iter()
        .enumerate()
        .filter(|(_, c)| c.entry.verification != crate::entry::VerificationState::RolledBack)
        .map(|(i, _)| i)
        .collect();
    let texts: Vec<String> = embedded
        .iter()
        .map(|&i| format!("{}\n{}", pool[i].entry.summary, pool[i].entry.body))
        .collect();
    let vectors = embedder.embed(&texts)?;
    let query_vecs = embedder.embed(std::slice::from_ref(&query.text))?;
    let qv = &query_vecs[0];
    let mut vec_scores = vec![0.0f32; pool.len()];
    for (i, v) in embedded.iter().zip(vectors.iter()) {
        vec_scores[*i] = cosine(qv, v);
    }
    let mut vec_rank: Vec<usize> = (0..pool.len())
        .filter(|&i| vec_scores[i] > VECTOR_MATCH_FLOOR)
        .collect();
    vec_rank.sort_by(|a, b| vec_scores[*b].total_cmp(&vec_scores[*a]));

    let fused = rrf_fuse(&[kw_rank, vec_rank], pool.len(), 60.0);

    // Topic match = signal in either channel. Coverage and hits both draw
    // from this set, so a vector-only paraphrase hit still lifts the
    // abstention signal.
    let mut relevance = vec![0.0f64; pool.len()];
    for (i, rrf) in &fused {
        relevance[*i] = rrf * 1000.0;
    }
    let matched: Vec<(crate::entry::Entry, recall::StateAt, f64)> = (0..pool.len())
        .filter(|&i| kw_scores[i] > 0.0 || vec_scores[i] > VECTOR_MATCH_FLOOR)
        .map(|i| (pool[i].entry.clone(), pool[i].state, relevance[i]))
        .collect();

    Ok(recall::finish(matched, &revised, query, pool_len, skipped))
}

/// The shipped embedder: bge-small-en-v1.5 through fastembed's ONNX runtime.
/// First use downloads ~100MB of model weights into `<root>/.cache/fastembed`
/// — the only network call gungnir ever makes, reachable only when the
/// `embed` feature is compiled in *and* a semantic query runs.
#[cfg(feature = "embed")]
pub mod local {
    use super::*;
    use std::sync::Mutex;

    /// Lazy holder: model weights load (and possibly download) on the first
    /// `embed` call, not at construction, so an MCP server that merely
    /// *might* answer a semantic query never pays the startup cost.
    pub struct Fastembed {
        cache_dir: PathBuf,
        inner: Mutex<Option<fastembed::TextEmbedding>>,
        model_name: String,
    }

    impl Fastembed {
        /// `root` is the gungnir memory root so weights share `.cache` with
        /// the vector cache.
        pub fn new(root: impl AsRef<Path>) -> Result<Self> {
            Ok(Self {
                cache_dir: root.as_ref().join(crate::layout::CACHE).join("fastembed"),
                inner: Mutex::new(None),
                model_name: "bge-small-en-v1.5-q".into(),
            })
        }
    }

    impl Embedder for Fastembed {
        fn model(&self) -> &str {
            &self.model_name
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            let mut slot = self
                .inner
                .lock()
                .map_err(|_| Error::Invalid("embedder poisoned".into()))?;
            if slot.is_none() {
                let options =
                    fastembed::TextInitOptions::new(fastembed::EmbeddingModel::BGESmallENV15Q)
                        .with_cache_dir(self.cache_dir.clone())
                        .with_show_download_progress(false);
                *slot = Some(
                    fastembed::TextEmbedding::try_new(options)
                        .map_err(|e| Error::Invalid(format!("embedder init failed: {e}")))?,
                );
            }
            slot.as_mut()
                .unwrap()
                .embed(texts, None)
                .map_err(|e| Error::Invalid(format!("embed failed: {e}")))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn construction_is_lazy_and_never_touches_the_network() {
            let dir = tempfile::tempdir().unwrap();
            let e = Fastembed::new(dir.path()).unwrap();
            assert_eq!(e.model(), "bge-small-en-v1.5-q");
            assert_eq!(
                e.cache_dir,
                dir.path().join(crate::layout::CACHE).join("fastembed")
            );
            assert!(e.inner.lock().unwrap().is_none(), "model stays unloaded");
            assert!(!e.cache_dir.exists(), "no files written before first embed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Entry, EntryKind, VerificationState};
    use std::sync::Mutex;

    /// Deterministic fake: vector = presence bits of known tokens.
    struct Fake {
        calls: Mutex<usize>,
    }
    const FAKE_TOKENS: &[&str] = &["postgres", "redis", "checkout", "index"];
    impl Embedder for Fake {
        fn model(&self) -> &str {
            "fake-1"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            *self.calls.lock().unwrap() += texts.len();
            Ok(texts
                .iter()
                .map(|t| {
                    FAKE_TOKENS
                        .iter()
                        .map(|tok| {
                            if t.to_lowercase().contains(tok) {
                                1.0
                            } else {
                                0.0
                            }
                        })
                        .collect()
                })
                .collect())
        }
    }

    #[test]
    fn cosine_direction_not_magnitude() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn rrf_prefers_candidates_ranked_well_everywhere() {
        // Candidate 0: 2nd in both lists. Candidate 1: 1st in one, absent in other.
        let fused = rrf_fuse(&[vec![1, 0, 2], vec![0, 2]], 3, 60.0);
        assert_eq!(fused[0].0, 0, "consistent runner-up wins fusion");
    }

    #[test]
    fn cache_hits_avoid_embedder_calls() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake {
            calls: Mutex::new(0),
        };
        let cached = CachedEmbedder::new(fake, dir.path());

        let v1 = cached.embed(&["postgres index".into()]).unwrap();
        let v2 = cached.embed(&["postgres   INDEX".into()]).unwrap(); // normalized equal
        assert_eq!(v1, v2);
        assert_eq!(
            *cached.inner.calls.lock().unwrap(),
            1,
            "second call served from cache"
        );

        // Cache file exists under model partition with content-derived name.
        let files: Vec<_> = walkdir::WalkDir::new(dir.path().join(".cache"))
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .collect();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn hybrid_drops_entries_with_no_signal_in_either_channel() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        // Matches the query by keyword and by vector.
        let mut match_both = Entry::new("a", EntryKind::Decision, "postgres connection reuse");
        match_both.body = "checkout of connections".into();
        // Zero keyword overlap and a zero vector under the fake embedder.
        let mut silent = Entry::new("a", EntryKind::Decision, "tape backup rotation");
        silent.body = "tape backup".into();
        store.create(&match_both).unwrap();
        store.create(&silent).unwrap();

        let fake = Fake {
            calls: Mutex::new(0),
        };
        let embedder = CachedEmbedder::new(fake, dir.path());
        let out = hybrid_search(&store, &Query::new("postgres checkout", 10), &embedder).unwrap();

        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.summary, "postgres connection reuse");
    }

    #[test]
    fn hybrid_returns_coverage_like_keyword_search() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut verified = Entry::new("a", EntryKind::Decision, "postgres pooling limits");
        verified.verify("ops", None);
        let unverified = Entry::new("a", EntryKind::Observation, "postgres timeout notes");
        store.create(&verified).unwrap();
        store.create(&unverified).unwrap();

        let embedder = CachedEmbedder::new(
            Fake {
                calls: Mutex::new(0),
            },
            dir.path(),
        );
        let out = hybrid_search(&store, &Query::new("postgres", 10), &embedder).unwrap();
        assert_eq!(out.coverage.verified, 1);
        assert_eq!(out.coverage.unverified, 1);
        assert_eq!(out.hits.len(), 2);
    }

    #[test]
    fn hybrid_honors_as_of_and_current_only() {
        // Regression: hybrid_search used to read only query.text/limit, so a
        // hybrid `as_of` or `current` query silently returned future and
        // superseded entries.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();

        let mut v1 = Entry::new("a", EntryKind::Decision, "checkout uses postgres");
        v1.timestamp = chrono::Utc::now() - chrono::Duration::days(10);
        store.create(&v1).unwrap();
        let mut v2 = Entry::new("a", EntryKind::Decision, "checkout uses redis");
        v2.revises = Some(v1.id);
        store.create(&v2).unwrap();
        let mut future = Entry::new("a", EntryKind::Decision, "checkout uses sqlite");
        future.timestamp = chrono::Utc::now() + chrono::Duration::days(10);
        store.create(&future).unwrap();

        let embedder = CachedEmbedder::new(
            Fake {
                calls: Mutex::new(0),
            },
            dir.path(),
        );

        // as_of before v2 and future exist: only v1 may appear.
        let out = hybrid_search(
            &store,
            &Query::new("checkout", 10).as_of(chrono::Utc::now() - chrono::Duration::days(5)),
            &embedder,
        )
        .unwrap();
        assert_eq!(out.hits.len(), 1);
        assert_eq!(out.hits[0].entry.summary, "checkout uses postgres");

        // current_only: the tail is hidden; the future entry has no
        // revises link so it stays a head. The assert that matters is v1.
        let out = hybrid_search(&store, &Query::new("checkout", 10).current(), &embedder).unwrap();
        let summaries: Vec<&str> = out.hits.iter().map(|h| h.entry.summary.as_str()).collect();
        assert!(summaries.contains(&"checkout uses redis"));
        assert!(!summaries.contains(&"checkout uses postgres"));
        assert_eq!(out.coverage.hidden_superseded, 1);
    }

    #[test]
    fn hybrid_vector_only_match_lifts_abstention() {
        // The paraphrase case: no keyword overlap, real vector signal.
        // Coverage must report the knowledge instead of claiming none.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut e = Entry::new("a", EntryKind::Decision, "postgres index choice");
        e.verify("ops", None);
        store.create(&e).unwrap();

        let embedder = CachedEmbedder::new(
            Fake {
                calls: Mutex::new(0),
            },
            dir.path(),
        );
        // Only the vector channel can hit: "postgresql" shares no stem with
        // the entry's tokens, so the keyword path must find nothing.
        assert!(
            recall::search_with_coverage(&store, &Query::new("postgresql", 10))
                .unwrap()
                .hits
                .is_empty(),
            "premise: keyword path misses"
        );
        let out = hybrid_search(&store, &Query::new("postgresql", 10), &embedder).unwrap();
        assert_eq!(out.coverage.verified, 1);
        assert_eq!(out.hits.len(), 1);
    }

    /// Vector similarity below the match floor contributes nothing: the
    /// entry is neither a hit nor coverage.
    struct BelowFloor;
    impl Embedder for BelowFloor {
        fn model(&self) -> &str {
            "below-floor-1"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.to_lowercase().contains("postgres") {
                        // cosine with the query's [1, 0] ≈ 0.30 < 0.45
                        vec![0.30f32, 0.9539]
                    } else {
                        vec![1.0f32, 0.0]
                    }
                })
                .collect())
        }
    }

    #[test]
    fn vector_match_below_the_floor_is_not_a_hit() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut e = Entry::new("a", EntryKind::Decision, "postgres index choice");
        e.verify("ops", None);
        store.create(&e).unwrap();

        let out = hybrid_search(&store, &Query::new("unrelated wording", 10), &BelowFloor).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage.total_visible(), 0);
    }

    #[test]
    fn hybrid_contradicted_never_outranks_verified() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut verified = Entry::new("a", EntryKind::Decision, "postgres index choice");
        verified.verify("ops", None);
        store.create(&verified).unwrap();
        let counter = Entry::new("a", EntryKind::Observation, "counter evidence");
        store.create(&counter).unwrap();
        let mut contradicted = Entry::new("a", EntryKind::Decision, "postgres index alternative");
        contradicted.contradict(counter.id, "ops", None);
        store.create(&contradicted).unwrap();

        let embedder = CachedEmbedder::new(
            Fake {
                calls: Mutex::new(0),
            },
            dir.path(),
        );
        let out = hybrid_search(&store, &Query::new("postgres index", 10), &embedder).unwrap();
        assert_eq!(out.hits[0].entry.verification, VerificationState::Verified);
    }

    #[test]
    fn state_at_is_evaluated_per_query_in_hybrid() {
        // Spot-check the shared temporal machinery sees the same states.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut e = Entry::new("a", EntryKind::Decision, "postgres config");
        e.mark_rolled_back("ops");
        store.create(&e).unwrap();

        let embedder = CachedEmbedder::new(
            Fake {
                calls: Mutex::new(0),
            },
            dir.path(),
        );
        let out = hybrid_search(&store, &Query::new("postgres", 10), &embedder).unwrap();
        assert!(out.hits.is_empty());
        assert_eq!(out.coverage.hidden_rolled_back, 1);
    }

    /// Records every text handed to it; rolled-back content must never
    /// reach an embedder, especially an external HTTP one.
    struct Recording {
        seen: Mutex<Vec<String>>,
    }
    impl Embedder for Recording {
        fn model(&self) -> &str {
            "rec-1"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.seen.lock().unwrap().extend(texts.iter().cloned());
            Ok(texts.iter().map(|_| vec![1.0f32]).collect())
        }
    }

    #[test]
    fn rolled_back_text_never_reaches_the_embedder() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let live = Entry::new("a", EntryKind::Decision, "postgres live fact");
        let mut rolled = Entry::new("a", EntryKind::Decision, "postgres retracted fact");
        rolled.mark_rolled_back("ops");
        store.create(&live).unwrap();
        store.create(&rolled).unwrap();

        let rec = Recording {
            seen: Mutex::new(vec![]),
        };
        let out = hybrid_search(&store, &Query::new("postgres", 10), &rec).unwrap();
        let seen = rec.seen.lock().unwrap();
        assert!(
            seen.iter().all(|t| !t.contains("retracted")),
            "retracted text went to the embedder: {seen:?}"
        );
        assert!(
            seen.iter().any(|t| t.contains("live fact")),
            "live entries still get embedded"
        );
        assert_eq!(out.coverage.hidden_rolled_back, 1);
    }

    /// An as_of query evaluates a rolled-back entry back to its
    /// pre-retraction state — and must still never send its text to the
    /// embedder. Egress is gated on the present state, not the as-of one.
    #[test]
    fn as_of_query_still_hides_retracted_text_from_the_embedder() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let mut rolled = Entry::new("a", EntryKind::Decision, "postgres retracted fact");
        // temporal_pool drops entries created after the cutoff entirely, so
        // the fixture must look old: created 3d ago, verified 2d ago,
        // rolled back now; the query lands in the verified window.
        rolled.timestamp = chrono::Utc::now() - chrono::Duration::days(3);
        rolled.verify("ops", None);
        rolled.verification_log[0].timestamp = chrono::Utc::now() - chrono::Duration::days(2);
        rolled.mark_rolled_back("ops");
        rolled.verification_log[1].timestamp = chrono::Utc::now();
        store.create(&rolled).unwrap();

        let rec = Recording {
            seen: Mutex::new(vec![]),
        };
        let mut q = Query::new("postgres retracted", 10);
        q = q.as_of(chrono::Utc::now() - chrono::Duration::days(1));
        let out = hybrid_search(&store, &q, &rec).unwrap();
        // Historical view: the entry was verified then, so it is a hit.
        assert_eq!(
            out.hits.len(),
            1,
            "as_of should surface the pre-rollback state"
        );
        let seen = rec.seen.lock().unwrap();
        assert!(
            seen.iter().all(|t| !t.contains("retracted fact")),
            "retracted text went to the embedder via as_of: {seen:?}"
        );
    }
}
