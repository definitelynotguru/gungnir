//! Embedding-based recall: pluggable embedder trait, content-addressed disk
//! cache, and reciprocal rank fusion for hybrid keyword + vector search.
//!
//! No HTTP client ships by default; implement [`Embedder`] against your
//! provider of choice and wrap it in [`CachedEmbedder`].

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::entry::Entry;
use crate::recall::{self, Hit, Query};
use crate::{Error, Result, Store};

/// Turns text into vectors. Implement this against your embedding provider.
pub trait Embedder: Send + Sync {
    fn model(&self) -> &str;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Disk cache in front of any embedder.
///
/// Cache key is `sha256(model + "\n" + normalized_text)` — content-addressed,
/// so editing a file invalidates its vector naturally and switching models
/// partitions the cache by directory. Vectors live under
/// `<store-root>/.cache/embeddings/<model>/<key>.json`.
pub struct CachedEmbedder<E: Embedder> {
    inner: E,
    cache_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct CachedVector {
    v: Vec<f32>,
}

impl<E: Embedder> CachedEmbedder<E> {
    pub fn new(inner: E, store_root: impl AsRef<Path>) -> Self {
        let model_dir = sanitize_model(inner.model());
        let cache_dir = store_root
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

/// Hybrid search: keyword ranking fused with embedding ranking via RRF.
/// Entries with no vector (empty body and summary) fall back to keyword-only.
pub fn hybrid_search(store: &Store, query: &Query, embedder: &dyn Embedder) -> Result<Vec<Hit>> {
    let entries = store.entries()?;
    let visible: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.verification != crate::entry::VerificationState::RolledBack)
        .collect();

    // Keyword ranking over the visible pool.
    let qtokens = {
        let mut t = recall::tokenize(&query.text);
        t.sort();
        t.dedup();
        t
    };
    let kw_scores: Vec<f64> = visible.iter().map(|e| recall::score(e, &qtokens)).collect();
    let mut kw_rank: Vec<usize> = (0..visible.len()).collect();
    kw_rank.sort_by(|a, b| kw_scores[*b].total_cmp(&kw_scores[*a]));

    // Vector ranking.
    let texts: Vec<String> = visible
        .iter()
        .map(|e| format!("{}\n{}", e.summary, e.body))
        .collect();
    let vectors = embedder.embed(&texts)?;
    let query_vecs = embedder.embed(std::slice::from_ref(&query.text))?;
    let qv = &query_vecs[0];
    let vec_scores: Vec<f32> = vectors.iter().map(|v| cosine(qv, v)).collect();
    let mut vec_rank: Vec<usize> = (0..visible.len()).collect();
    vec_rank.sort_by(|a, b| vec_scores[*b].total_cmp(&vec_scores[*a]));

    let fused = rrf_fuse(&[kw_rank, vec_rank], visible.len(), 60.0);

    let mut hits: Vec<Hit> = fused
        .into_iter()
        .filter(|(i, _)| kw_scores[*i] > 0.0 || vec_scores[*i] > 0.01)
        .take(query.limit)
        .map(|(i, score)| Hit {
            entry: visible[i].clone(),
            score,
        })
        .collect();

    hits.sort_by(|a, b| {
        recall::bucket(&b.entry)
            .cmp(&recall::bucket(&a.entry))
            .then(b.score.total_cmp(&a.score))
    });
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EntryKind;
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
        let hits = hybrid_search(&store, &Query::new("postgres checkout", 10), &embedder).unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].entry.summary, "postgres connection reuse");
    }
}
