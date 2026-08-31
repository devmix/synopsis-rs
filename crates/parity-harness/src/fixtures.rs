//! Fixture loader API (design D6).
//!
//! Parity fixtures are exported once from the Go oracle (`../synopsis`): a copy
//! of `knowledge.db` (SQLite, schema from the five oracle migrations) plus
//! `vectors.bin`, a binary vector dump in the SYNX format fixed by the
//! `vectors` change (native-seam-spikes design D4):
//!
//! ```text
//! magic "SYNX" · version u32 LE = 1 · dim u32 LE · count u64 LE
//! rows [u32 LE chunk_id][f32 LE × dim] × count, ascending chunk_id
//! ```
//!
//! [`FixtureSet::load`] validates that a fixture set exists on disk and exposes
//! its paths; [`FixtureSet::load_vectors`] opens the dump as a streaming row
//! iterator over `vectors::synx` — the ~4 GB target file is never loaded whole.
//!
//! [`load_fixture_set_from_dir`] loads the committed vector dump
//! (`vectors.bin` alone — the Go `knowledge.db` is never committed and never
//! opened, design D5 of `parity-harness-real-fixtures`) into memory as
//! `(chunk_id, vector)` rows; it is the input of the recall@k differential
//! test (design D3).

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::HarnessError;

/// File names expected in a fixture directory.
const KNOWNLEDGE_DB: &str = "knowledge.db";
const VECTORS_BIN: &str = "vectors.bin";

/// One exported fixture set: paths to the knowledge database and vector dump.
#[derive(Debug, Clone)]
pub struct FixtureSet {
    /// Path to the `knowledge.db` SQLite file (oracle schema, read-only).
    pub db_path: PathBuf,
    /// Path to the `vectors.bin` SYNX binary vector dump (`vectors::synx`).
    pub vectors_path: PathBuf,
}

impl FixtureSet {
    /// Load a fixture set from `dir`, which must contain both `knowledge.db`
    /// and `vectors.bin`.
    pub fn load(dir: impl Into<PathBuf>) -> Result<Self, HarnessError> {
        let dir = dir.into();
        for name in [KNOWNLEDGE_DB, VECTORS_BIN] {
            let path = dir.join(name);
            if !path.is_file() {
                return Err(HarnessError::Fixture {
                    reason: format!("file not found (expected inside {})", dir.display()),
                    path,
                });
            }
        }
        Ok(Self {
            db_path: dir.join(KNOWNLEDGE_DB),
            vectors_path: dir.join(VECTORS_BIN),
        })
    }

    /// Directory containing this fixture set (empty relative path if `db_path`
    /// has no parent component; never panics).
    pub fn dir(&self) -> &Path {
        self.db_path.parent().unwrap_or(Path::new(""))
    }

    /// Open the `vectors.bin` SYNX dump as a streaming row iterator.
    ///
    /// Yields `(chunk_id, vector)` rows one at a time in file order (ascending
    /// `chunk_id`); memory stays bounded by one row plus the read buffer, so a
    /// multi-GB fixture is never loaded whole. Header metadata is available via
    /// [`FixtureVectors::dim`] and [`FixtureVectors::row_count`].
    ///
    /// Every failure — missing/unreadable file, malformed SYNX header or row
    /// (bad magic, unsupported version, zero dim, truncation) — surfaces as
    /// [`HarnessError::Fixture`] carrying the file path.
    pub fn load_vectors(&self) -> Result<FixtureVectors, HarnessError> {
        let file = File::open(&self.vectors_path).map_err(|err| HarnessError::Fixture {
            path: self.vectors_path.clone(),
            reason: format!("cannot open: {err}"),
        })?;
        let reader = vectors::synx::open(file).map_err(|err| HarnessError::Fixture {
            path: self.vectors_path.clone(),
            reason: err.to_string(),
        })?;
        Ok(FixtureVectors {
            reader,
            path: self.vectors_path.clone(),
        })
    }
}

/// Streaming SYNX row iterator over a fixture's `vectors.bin` (see
/// [`FixtureSet::load_vectors`]).
///
/// Wraps the reader from `vectors::synx` so that row-level format errors
/// (e.g. a row truncated mid-file) carry the harness error type with the
/// fixture path instead of the engine crate's error type.
#[derive(Debug)]
pub struct FixtureVectors {
    reader: vectors::synx::SynxReader<File>,
    path: PathBuf,
}

impl FixtureVectors {
    /// Vector dimensionality from the SYNX header.
    pub fn dim(&self) -> u32 {
        self.reader.dim()
    }

    /// Total number of rows promised by the SYNX header.
    pub fn row_count(&self) -> u64 {
        self.reader.row_count()
    }
}

impl Iterator for FixtureVectors {
    type Item = Result<(u32, Vec<f32>), HarnessError>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.reader.next()?.map_err(|err| HarnessError::Fixture {
            path: self.path.clone(),
            reason: err.to_string(),
        }))
    }
}

/// Loads the SYNX vector dump `dir/vectors.bin` into memory as
/// `(chunk_id, vector)` rows in file order (ascending `chunk_id`, a format
/// requirement).
///
/// Unlike [`FixtureSet::load`], only the vector dump is required: the
/// committed parity fixture set ships `vectors.bin` alone (the Go
/// `knowledge.db` is never committed and never opened — legacy DB rule,
/// design D5 of `parity-harness-real-fixtures`).
///
/// Every failure — missing/unreadable file, malformed SYNX header or row
/// (bad magic, unsupported version, zero dim, truncation) — surfaces as
/// [`HarnessError::Fixture`] carrying the file path.
pub fn load_fixture_set_from_dir(
    dir: impl Into<PathBuf>,
) -> Result<Vec<(u32, Vec<f32>)>, HarnessError> {
    let path = dir.into().join(VECTORS_BIN);
    let file = File::open(&path).map_err(|err| HarnessError::Fixture {
        path: path.clone(),
        reason: format!("cannot open: {err}"),
    })?;
    let reader = vectors::synx::open(file).map_err(|err| HarnessError::Fixture {
        path: path.clone(),
        reason: err.to_string(),
    })?;
    reader
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| HarnessError::Fixture {
            path,
            reason: err.to_string(),
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Scratch fixture dir under the system temp area (never real user data).
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("parity-harness-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn load_missing_files_is_typed_fixture_error() {
        let dir = scratch_dir("missing");
        let err = FixtureSet::load(&dir).unwrap_err();
        match &err {
            HarnessError::Fixture { path, reason } => {
                assert!(reason.contains("not found"), "got: {reason}");
                assert!(path.file_name().is_some());
            }
            other => panic!("expected Fixture error, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_present_files_exposes_paths() {
        let dir = scratch_dir("present");
        std::fs::write(dir.join(KNOWNLEDGE_DB), DB_PLACEHOLDER).expect("write db placeholder");
        write_synx_file(dir.join(VECTORS_BIN), 16, &tiny_rows());

        let set = FixtureSet::load(&dir).unwrap();
        assert_eq!(set.db_path, dir.join(KNOWNLEDGE_DB));
        assert_eq!(set.vectors_path, dir.join(VECTORS_BIN));
        assert_eq!(set.dir(), dir.as_path());

        // Removing one file makes the set unloadable again.
        std::fs::remove_file(set.vectors_path.as_path()).expect("remove");
        assert!(FixtureSet::load(&dir).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Placeholder bytes for the `knowledge.db` in scratch fixture dirs: the
    /// existence checks under test never parse the file.
    const DB_PLACEHOLDER: &[u8] = b"sqlite-placeholder";

    /// Writes `rows` as a SYNX file at `path` (test helper; panics are
    /// intentional — writing to a scratch path cannot fail in practice).
    fn write_synx_file(path: impl AsRef<Path>, dim: u32, rows: &[(u32, Vec<f32>)]) {
        let mut file = File::create(path.as_ref()).expect("create vectors.bin");
        vectors::synx::write(
            &mut file,
            dim,
            rows.iter().map(|(id, vector)| (*id, vector.as_slice())),
        )
        .expect("write SYNX rows");
    }

    /// Write a tiny SYNX fixture set (placeholder db + generated vectors.bin)
    /// into a fresh scratch dir and return its path.
    fn write_fixture_set(tag: &str, dim: u32, rows: &[(u32, Vec<f32>)]) -> PathBuf {
        let dir = scratch_dir(tag);
        std::fs::write(dir.join(KNOWNLEDGE_DB), DB_PLACEHOLDER).expect("write db placeholder");
        write_synx_file(dir.join(VECTORS_BIN), dim, rows);
        dir
    }

    /// Deterministic rows: 8 × 16-dim, distinct values, ids out of order so the
    /// writer's ascending-chunk_id sort is exercised.
    fn tiny_rows() -> Vec<(u32, Vec<f32>)> {
        (0..8u32)
            .map(|i| {
                let id = [5, 1, 9, 3, 7, 2, 10, 4][i as usize];
                let vector = (0..16).map(|j| (i * 17 + j) as f32 / 10.0).collect();
                (id, vector)
            })
            .collect()
    }

    #[test]
    fn load_vectors_streams_rows_in_chunk_id_order() {
        let rows = tiny_rows();
        let dir = write_fixture_set("vectors-roundtrip", 16, &rows);

        let set = FixtureSet::load(&dir).unwrap();
        let reader = set.load_vectors().unwrap();
        assert_eq!(reader.dim(), 16);
        assert_eq!(reader.row_count(), 8);

        let loaded: Vec<(u32, Vec<f32>)> = reader.collect::<Result<_, _>>().unwrap();
        // Format requirement: rows come back sorted by ascending chunk_id.
        let ids: Vec<u32> = loaded.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [1, 2, 3, 4, 5, 7, 9, 10]);
        // Values survive the roundtrip exactly (bit-for-bit f32).
        for (id, vector) in &loaded {
            let source = rows.iter().find(|(rid, _)| rid == id).unwrap();
            assert_eq!(vector, &source.1, "id {id}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_vectors_rejects_malformed_dump() {
        let rows = tiny_rows();
        let dir = write_fixture_set("vectors-malformed", 16, &rows);
        let path = dir.join(VECTORS_BIN);
        let bytes = std::fs::read(&path).expect("read fixture");

        // Bad magic: first byte flipped.
        let mut bad_magic = bytes.clone();
        bad_magic[0] ^= 0xFF;
        std::fs::write(&path, bad_magic).expect("write bad magic");
        let set = FixtureSet::load(&dir).unwrap();
        match set.load_vectors().unwrap_err() {
            HarnessError::Fixture { path, reason } => {
                assert_eq!(path, dir.join(VECTORS_BIN));
                assert!(reason.contains("magic"), "got: {reason}");
            }
            other => panic!("expected Fixture error, got {other:?}"),
        }

        // Truncated: last row cut short.
        std::fs::write(&path, &bytes[..bytes.len() - 3]).expect("write truncated");
        let set = FixtureSet::load(&dir).unwrap();
        let err = set.load_vectors().unwrap().collect::<Result<Vec<_>, _>>();
        match err.unwrap_err() {
            HarnessError::Fixture { reason, .. } => {
                assert!(reason.contains("truncated"), "got: {reason}");
            }
            other => panic!("expected Fixture error, got {other:?}"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end (task vectors 1.7): generate a small SYNX fixture with
    /// `vectors::synx::write`, load it back through the harness loader, and
    /// score exact-L2 brute-force top-k against itself via `recall_at_k`.
    /// No network, no real user data — everything is generated in a scratch dir.
    #[test]
    fn synx_roundtrip_recall_against_brute_force() {
        use crate::metrics::recall_at_k;

        const DIM: usize = 8;
        const K: usize = 3;
        // 12 rows with distinct values (no distance ties) and out-of-order ids.
        let rows: Vec<(u32, Vec<f32>)> = (0..12usize)
            .map(|i| {
                let id = (i * 5) as u32;
                let vector = (0..DIM).map(|j| (i * 31 + j * 7 + 1) as f32).collect();
                (id, vector)
            })
            .collect();
        // 3 held-out queries, never part of the corpus.
        let queries: Vec<Vec<f32>> = (0..3usize)
            .map(|i| (0..DIM).map(|j| (i * 13 + j * 11 + 5) as f32).collect())
            .collect();

        let dir = write_fixture_set("synx-e2e", DIM as u32, &rows);
        let set = FixtureSet::load(&dir).unwrap();
        let reader = set.load_vectors().unwrap();
        assert_eq!(reader.dim() as usize, DIM);
        assert_eq!(reader.row_count(), 12);
        let loaded: Vec<(u32, Vec<f32>)> = reader.collect::<Result<_, _>>().unwrap();
        let ids: Vec<u32> = loaded.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, (0..12u32).map(|i| i * 5).collect::<Vec<_>>());

        // Ground truth: exact squared-L2 distances per query (f64
        // accumulation), sorted ascending. Distinct values guarantee no ties.
        let exact: Vec<Vec<(f64, u32)>> = queries
            .iter()
            .map(|query| {
                let mut dists: Vec<(f64, u32)> = loaded
                    .iter()
                    .map(|(id, vector)| {
                        let acc = query
                            .iter()
                            .zip(vector)
                            .map(|(a, b)| {
                                let d = *a as f64 - *b as f64;
                                d * d
                            })
                            .sum();
                        (acc, *id)
                    })
                    .collect();
                dists.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
                dists
            })
            .collect();
        // Top-K = ground truth; bottom-K = the farthest rows (disjoint from
        // the top-K of a 12-row corpus at K = 3).
        let ground_truth: Vec<Vec<u32>> = exact
            .iter()
            .map(|dists| dists.iter().take(K).map(|(_, id)| *id).collect())
            .collect();
        let farthest: Vec<Vec<u32>> = exact
            .iter()
            .map(|dists| dists.iter().rev().take(K).map(|(_, id)| *id).collect())
            .collect();
        assert!(
            !ground_truth[0].iter().any(|id| farthest[0].contains(id)),
            "top-K and bottom-K must be disjoint here"
        );

        // Candidates from the same exact computation: full plumbing check.
        let recall = recall_at_k(&ground_truth, &ground_truth).unwrap();
        assert!(
            (recall - 1.0).abs() < 1e-12,
            "self-comparison is perfect, got {recall}"
        );

        // Farthest-first candidates score exactly zero.
        assert_eq!(recall_at_k(&farthest, &ground_truth).unwrap(), 0.0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real-fixture recall gate (task 1.1, design D3): load the committed
    /// `fixtures/vectors.bin` (SYNX, dim=384, 270 rows extracted from the Go
    /// oracle's vec0 — task 1.2), build a UsearchEngine ANN index over it in
    /// a scratch dir, and score the engine's top-10 against exact-L2
    /// brute-force ground truth computed here. The committed fixture is only
    /// ever READ, never regenerated or overwritten.
    ///
    /// Gate: recall@10 >= 0.95 relative to brute-force ground truth
    /// (`openspec/specs/data-schema/spec.md`).
    #[test]
    fn recall_at_k_on_real_fixture_meets_the_gate() {
        use crate::metrics::recall_at_k;
        use vectors::{UsearchEngine, VectorIndexConfig};

        const K: usize = 10;
        // 20 queries spread across the corpus: fixture rows 0, 13, ..., 247.
        const QUERY_STRIDE: usize = 13;
        const QUERY_COUNT: usize = 20;
        const RECALL_GATE: f64 = 0.95;

        // The committed fixture (task 1.2), read from the crate's fixtures dir.
        let fixture_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let rows = load_fixture_set_from_dir(&fixture_dir).expect("committed fixture loads");
        assert_eq!(rows.len(), 270, "fixture row count");
        assert!(
            rows.iter().all(|(_, vector)| vector.len() == 384),
            "fixture dim is 384"
        );

        // ANN index over the fixture in a scratch dir. ef_search larger than
        // the 270-row corpus: the HNSW search is exhaustive, so recall must
        // be (near) perfect.
        let dir = scratch_dir("recall-usearch");
        let config = VectorIndexConfig::new(384, 16, 100, 300).expect("config is valid");
        let engine = UsearchEngine::create(dir.join("usearch"), config).expect("create engine");
        let refs: Vec<(u32, &[f32])> = rows
            .iter()
            .map(|(id, vector)| (*id, vector.as_slice()))
            .collect();
        engine.insert_batch(&refs).expect("insert fixture rows");
        engine.build_index().expect("build index");
        assert_eq!(engine.count().expect("count"), 270);

        // Queries: 20 fixture vectors, spread by stride (each query is a
        // stored row, so its own id is the exact top-1).
        let queries: Vec<&Vec<f32>> = rows
            .iter()
            .step_by(QUERY_STRIDE)
            .take(QUERY_COUNT)
            .map(|(_, vector)| vector)
            .collect();
        assert_eq!(queries.len(), QUERY_COUNT, "query count");

        // Ground truth: exact squared-L2 top-K per query (f64 accumulation),
        // ties broken by chunk id.
        let ground_truth: Vec<Vec<u32>> = queries
            .iter()
            .map(|query| {
                let mut dists: Vec<(f64, u32)> = rows
                    .iter()
                    .map(|(id, vector)| {
                        let acc = query
                            .iter()
                            .zip(vector)
                            .map(|(a, b)| {
                                let d = *a as f64 - *b as f64;
                                d * d
                            })
                            .sum();
                        (acc, *id)
                    })
                    .collect();
                dists.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
                dists.into_iter().take(K).map(|(_, id)| id).collect()
            })
            .collect();

        let candidates: Vec<Vec<u32>> = queries
            .iter()
            .map(|query| {
                engine
                    .search(query, K)
                    .expect("search")
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect()
            })
            .collect();

        let recall = recall_at_k(&candidates, &ground_truth).expect("recall defined");
        assert!(
            recall >= RECALL_GATE,
            "recall@{K} = {recall} is below the {RECALL_GATE} gate"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
