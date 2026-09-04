//! Deterministic synthetic data generation (design D11).
//!
//! Uses SplitMix64 (the `rand` crate is not in the frozen workspace
//! palette); per-seed determinism is preserved.

use std::collections::HashMap;

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Number of benchmark argument samples per collection.
pub const DEFAULT_SAMPLES_SIZE: usize = 32;

/// The ordered list of domains the generated data spans.
pub const DOMAINS: [&str; 5] = ["hr", "product", "engineering", "finance", "security"];

/// Predefined dataset sizes.
#[derive(Debug, Clone, Serialize)]
pub struct Scale {
    /// Scale name.
    pub name: String,
    /// Number of documents.
    pub documents: usize,
    /// Number of chunks.
    pub chunks: usize,
    /// Number of entities.
    pub entities: usize,
    /// Number of facts.
    pub facts: usize,
}

impl Scale {
    /// Parses a scale by name.
    pub fn parse(name: &str) -> Result<Self, String> {
        match name.trim().to_lowercase().as_str() {
            "small" => Ok(Self {
                name: "small".into(),
                documents: 500,
                chunks: 10_000,
                entities: 2_000,
                facts: 5_000,
            }),
            "medium" => Ok(Self {
                name: "medium".into(),
                documents: 5_000,
                chunks: 100_000,
                entities: 20_000,
                facts: 10_000,
            }),
            "large" => Ok(Self {
                name: "large".into(),
                documents: 10_000,
                chunks: 200_000,
                entities: 40_000,
                facts: 20_000,
            }),
            other => Err(format!(
                "unknown scale {other:?}, want one of: small, medium, large"
            )),
        }
    }
}

/// A generated source document.
#[derive(Debug, Clone)]
pub struct Document {
    /// Document ID.
    pub id: u32,
    /// Source type.
    pub source_type: String,
    /// Original path.
    pub original_path: String,
    /// Primary domain.
    pub domain: String,
    /// Metadata JSON.
    pub metadata_json: String,
    /// Content hash (hex SHA-256).
    pub content_hash: Option<String>,
}

/// A generated text chunk.
#[derive(Debug, Clone)]
pub struct Chunk {
    /// Chunk ID.
    pub id: u32,
    /// Document ID.
    pub doc_id: u32,
    /// Sequence number.
    pub seq_num: u32,
    /// Chunk text.
    pub text: String,
    /// Start offset.
    pub start_offset: Option<u32>,
    /// End offset.
    pub end_offset: Option<u32>,
}

/// A generated entity.
#[derive(Debug, Clone)]
pub struct Entity {
    /// Entity ID.
    pub id: u32,
    /// Entity type.
    pub entity_type: String,
    /// Entity name.
    pub name: String,
    /// Domain.
    pub domain: String,
    /// Description.
    pub description: String,
    /// Confidence.
    pub confidence: f64,
}

/// A generated fact.
#[derive(Debug, Clone)]
pub struct Fact {
    /// Fact ID.
    pub id: u32,
    /// Subject entity ID.
    pub subject_id: u32,
    /// Predicate.
    pub predicate: String,
    /// Object entity ID.
    pub object_id: u32,
    /// Domain.
    pub domain: String,
    /// Status.
    pub status: String,
    /// Valid from.
    pub valid_from: Option<String>,
    /// Valid to.
    pub valid_to: Option<String>,
    /// Weight.
    pub weight: u32,
}

/// A fact-to-document source link.
#[derive(Debug, Clone)]
pub struct FactSource {
    /// Fact ID.
    pub fact_id: u32,
    /// Document ID.
    pub document_id: u32,
    /// Quote.
    pub quote: String,
}

/// A cross-domain entity link.
#[derive(Debug, Clone)]
pub struct EntityLink {
    /// Subject entity ID.
    pub subject_id: u32,
    /// Target entity ID.
    pub target_id: u32,
    /// Relation type.
    pub relation_type: String,
    /// Method.
    pub method: String,
    /// Confidence.
    pub confidence: f64,
    /// Evidence.
    pub evidence: String,
}

/// A chunk-to-entity link.
#[derive(Debug, Clone)]
pub struct ChunkEntity {
    /// Chunk ID.
    pub chunk_id: u32,
    /// Entity ID.
    pub entity_id: u32,
}

/// An entity-to-document source link.
#[derive(Debug, Clone)]
pub struct EntitySource {
    /// Entity ID.
    pub entity_id: u32,
    /// Document ID.
    pub document_id: u32,
}

/// A complete generated dataset.
#[derive(Debug, Clone)]
pub struct Dataset {
    /// Scale.
    pub scale: Scale,
    /// Seed.
    pub seed: i64,
    /// Documents.
    pub documents: Vec<Document>,
    /// Chunks.
    pub chunks: Vec<Chunk>,
    /// Entities.
    pub entities: Vec<Entity>,
    /// Facts.
    pub facts: Vec<Fact>,
    /// Fact sources.
    pub fact_sources: Vec<FactSource>,
    /// Entity links.
    pub entity_links: Vec<EntityLink>,
    /// Chunk entities.
    pub chunk_entities: Vec<ChunkEntity>,
    /// Entity sources.
    pub entity_sources: Vec<EntitySource>,
    /// Benchmark samples.
    pub samples: Samples,
}

/// Deterministic benchmark argument samples.
#[derive(Debug, Clone)]
pub struct Samples {
    /// Search queries.
    pub queries: Vec<String>,
    /// Document IDs.
    pub doc_ids: Vec<u32>,
    /// Chunk IDs.
    pub chunk_ids: Vec<u32>,
    /// Fact IDs.
    pub fact_ids: Vec<u32>,
    /// Entity IDs.
    pub entity_ids: Vec<u32>,
    /// Entity types.
    pub entity_types: Vec<String>,
}

/// SplitMix64 PRNG (deterministic, no external crate).
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Creates a PRNG with the given seed.
    fn new(seed: i64) -> Self {
        Self { state: seed as u64 }
    }

    /// Next u64.
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform integer in `[0, n)`.
    fn intn(&mut self, n: usize) -> usize {
        (self.next_u64() as usize) % n
    }

    /// Uniform float in `[0, 1)`.
    fn float64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Fisher-Yates shuffle.
    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.intn(i + 1);
            items.swap(i, j);
        }
    }
}

/// Deterministic dataset generator.
pub struct Generator {
    rng: SplitMix64,
}

impl Generator {
    /// Creates a generator with the given PRNG seed.
    pub fn new(seed: i64) -> Self {
        Self {
            rng: SplitMix64::new(seed),
        }
    }

    /// Builds a complete dataset for the given scale.
    pub fn generate(&mut self, scale: &Scale) -> Result<Dataset, String> {
        if scale.documents == 0 || scale.chunks < scale.documents {
            return Err(format!(
                "invalid scale: need at least one chunk per document (documents={}, chunks={})",
                scale.documents, scale.chunks
            ));
        }
        if scale.documents < DOMAINS.len() || scale.entities < DOMAINS.len() {
            return Err(format!(
                "invalid scale {:?}: need at least one document and one entity per domain",
                scale.name
            ));
        }

        let mut ds = Dataset {
            scale: scale.clone(),
            seed: self.rng.state as i64,
            documents: Vec::new(),
            chunks: Vec::new(),
            entities: Vec::new(),
            facts: Vec::new(),
            fact_sources: Vec::new(),
            entity_links: Vec::new(),
            chunk_entities: Vec::new(),
            entity_sources: Vec::new(),
            samples: Samples {
                queries: Vec::new(),
                doc_ids: Vec::new(),
                chunk_ids: Vec::new(),
                fact_ids: Vec::new(),
                entity_ids: Vec::new(),
                entity_types: Vec::new(),
            },
        };

        self.generate_documents(&mut ds)?;
        self.generate_chunks(&mut ds)?;
        self.generate_entities(&mut ds)?;
        self.generate_chunk_entities(&mut ds);
        self.generate_facts(&mut ds)?;
        self.generate_fact_sources(&mut ds);
        self.generate_entity_sources(&mut ds);
        self.generate_entity_links(&mut ds)?;
        self.build_samples(&mut ds);

        Ok(ds)
    }

    fn generate_documents(&mut self, ds: &mut Dataset) -> Result<(), String> {
        let base = ds.scale.documents / DOMAINS.len();
        let mut id = 0u32;
        for (di, &domain) in DOMAINS.iter().enumerate() {
            let count = if di < ds.scale.documents % DOMAINS.len() {
                base + 1
            } else {
                base
            };
            let vocab = domain_vocabulary(domain);
            for n in 0..count {
                id += 1;
                let (ext, source_type) = if (n + di) % 5 == 4 {
                    ("json", "json")
                } else {
                    ("md", "markdown")
                };
                let t1 = vocab[self.rng.intn(vocab.len())];
                let t2 = vocab[self.rng.intn(vocab.len())];
                let title = format!("{} and {}: internal guidelines", title_case(t1), t2);
                ds.documents.push(Document {
                    id,
                    source_type: source_type.to_owned(),
                    original_path: format!("/synthetic/{domain}/doc-{id:06}.{ext}"),
                    domain: domain.to_owned(),
                    metadata_json: format!(
                        r#"{{"title":{},"domain":[{}]}}"#,
                        serde_json::to_string(&title).unwrap_or_default(),
                        serde_json::to_string(domain).unwrap_or_default()
                    ),
                    content_hash: None,
                });
            }
        }
        if id != ds.scale.documents as u32 {
            return Err(format!(
                "generated {id} documents, want {}",
                ds.scale.documents
            ));
        }
        Ok(())
    }

    fn generate_chunks(&mut self, ds: &mut Dataset) -> Result<(), String> {
        let base = ds.scale.chunks / ds.scale.documents;
        let remainder = ds.scale.chunks % ds.scale.documents;
        let mut chunk_id = 0u32;

        for (i, doc) in ds.documents.iter_mut().enumerate() {
            let count = if i < remainder { base + 1 } else { base };
            if count == 0 {
                return Err(format!(
                    "document {} got a non-positive chunk count",
                    doc.id
                ));
            }

            let mut full_text = String::new();
            for seq in 1..=count {
                chunk_id += 1;
                let text = self.chunk_text(&doc.domain);
                let start = full_text.len() as u32;
                if start > 0 {
                    full_text.push_str("\n\n");
                }
                full_text.push_str(&text);
                let end = full_text.len() as u32;
                ds.chunks.push(Chunk {
                    id: chunk_id,
                    doc_id: doc.id,
                    seq_num: seq as u32,
                    text,
                    start_offset: Some(start),
                    end_offset: Some(end),
                });
            }
            let mut hasher = Sha256::new();
            hasher.update(full_text.as_bytes());
            doc.content_hash = Some(
                hasher
                    .finalize()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            );
        }

        if chunk_id != ds.scale.chunks as u32 {
            return Err(format!(
                "generated {chunk_id} chunks, want {}",
                ds.scale.chunks
            ));
        }
        Ok(())
    }

    fn generate_entities(&mut self, ds: &mut Dataset) -> Result<(), String> {
        let base = ds.scale.entities / DOMAINS.len();
        let mut id = 0u32;
        let mut used_names: HashMap<String, bool> = HashMap::new();

        for (di, &domain) in DOMAINS.iter().enumerate() {
            let count = if di < ds.scale.entities % DOMAINS.len() {
                base + 1
            } else {
                base
            };
            let types = entity_types_by_domain(domain);
            let vocab = domain_vocabulary(domain);
            for n in 0..count {
                id += 1;
                let entity_type = types[n % types.len()];
                let key_prefix = format!("{domain}|{entity_type}");
                let name = if entity_type == "employee" {
                    self.person_name(&mut used_names, &key_prefix)
                } else {
                    let base_name = format!(
                        "{} {}",
                        title_case(entity_type),
                        vocab[self.rng.intn(vocab.len())]
                    );
                    let mut name = base_name.clone();
                    let mut k = 2;
                    while used_names.contains_key(&format!("{key_prefix}|{name}")) {
                        name = format!("{base_name}-{k}");
                        k += 1;
                    }
                    name
                };

                let key = format!("{key_prefix}|{name}");
                if used_names.contains_key(&key) {
                    return Err(format!("duplicate entity generated: {key}"));
                }
                used_names.insert(key, true);

                ds.entities.push(Entity {
                    id,
                    entity_type: entity_type.to_owned(),
                    name: name.clone(),
                    domain: domain.to_owned(),
                    description: format!(
                        "{} entity in the {domain} domain covering {}.",
                        title_case(entity_type),
                        vocab[(n + 3) % vocab.len()]
                    ),
                    confidence: round4(0.7 + self.rng.float64() * 0.29),
                });
            }
        }

        if id != ds.scale.entities as u32 {
            return Err(format!(
                "generated {id} entities, want {}",
                ds.scale.entities
            ));
        }
        Ok(())
    }

    fn person_name(&mut self, used: &mut HashMap<String, bool>, prefix: &str) -> String {
        let first_names = [
            "Alice", "Bob", "Carol", "David", "Elena", "Frank", "Grace", "Henry", "Irina", "Jack",
            "Kira", "Leo", "Maria", "Nikolai", "Olga", "Peter", "Quinn", "Rosa", "Sergei", "Tina",
            "Umar", "Vera", "Walter", "Xena", "Yuri", "Zoe", "Adam", "Bella", "Cyril", "Diana",
        ];
        let last_names = [
            "Anderson", "Baker", "Chen", "Dmitriev", "Evans", "Fischer", "Garcia", "Hoffman",
            "Ivanov", "Johnson", "Kim", "Larsen", "Morozova", "Novak", "Olsen", "Petrov", "Quist",
            "Romanov", "Schmidt", "Tanaka", "Ueda", "Volkov", "Wagner", "Xu", "Young", "Zaytsev",
            "Bergman", "Costa", "Dubois", "Eriksen",
        ];
        let mut k = 0u32;
        loop {
            let base = format!(
                "{} {}",
                first_names[self.rng.intn(first_names.len())],
                last_names[self.rng.intn(last_names.len())]
            );
            let name = if k > 0 { format!("{base} {k}") } else { base };
            if !used.contains_key(&format!("{prefix}|{name}")) {
                return name;
            }
            k += 1;
        }
    }

    fn generate_facts(&mut self, ds: &mut Dataset) -> Result<(), String> {
        let entity_range = id_ranges_by_domain(&ds.entities, |e| &e.domain);
        let mut fact_index: HashMap<&str, u32> = HashMap::new();

        for i in 1..=ds.scale.facts as u32 {
            let domain = DOMAINS[(i as usize - 1) % DOMAINS.len()];
            let &(lo, hi) = entity_range
                .get(domain)
                .ok_or_else(|| format!("no range for {domain}"))?;
            let e_count = (hi - lo) as i64;
            let preds = predicates_by_domain(domain);
            let j = fact_index.entry(domain).or_insert(0);
            *j += 1;
            let j = *j - 1;

            let combinations = e_count * preds.len() as i64 * e_count;
            if combinations <= 0 {
                return Err(format!("no entity combinations in domain {domain:?}"));
            }
            let idx = (j as i64 * 7919) % combinations;
            let subject_offset = (idx % e_count) as u32;
            let t = idx / e_count;
            let pred_idx = (t % preds.len() as i64) as usize;
            let object_offset = ((t / preds.len() as i64) % e_count) as u32;

            let status = match self.rng.float64() {
                r if r >= 0.97 => "rejected",
                r if r >= 0.87 => "draft",
                r if r >= 0.72 => "pending",
                _ => "approved",
            };

            let mut valid_from = None;
            if self.rng.float64() < 0.5 {
                valid_from = Some(format!(
                    "20{}-{:02}-15",
                    self.rng.intn(4) + 3,
                    self.rng.intn(12) + 1
                ));
            }
            let valid_to = if valid_from.is_some() && self.rng.float64() < 0.3 {
                Some(format!(
                    "20{}-{:02}-30",
                    self.rng.intn(4) + 5,
                    self.rng.intn(12) + 1
                ))
            } else {
                None
            };

            ds.facts.push(Fact {
                id: i,
                subject_id: lo + subject_offset,
                predicate: preds[pred_idx].to_owned(),
                object_id: lo + object_offset,
                domain: domain.to_owned(),
                status: status.to_owned(),
                valid_from,
                valid_to,
                weight: self.rng.intn(3) as u32 + 1,
            });
        }
        Ok(())
    }

    fn generate_fact_sources(&mut self, ds: &mut Dataset) {
        let doc_range = doc_ranges_by_domain(ds);
        let entries: Vec<(u32, u32, u32, String)> = ds
            .facts
            .iter()
            .map(|f| {
                let &(lo, hi) = doc_range.get(f.domain.as_str()).unwrap_or(&(1, 2));
                (f.id, lo, hi, f.domain.clone())
            })
            .collect();
        drop(doc_range);
        for (fact_id, lo, hi, domain) in entries {
            let vocab = domain_vocabulary(&domain);
            let quote = format!(
                "The {} procedure must be documented and reviewed.",
                vocab[self.rng.intn(vocab.len())]
            );
            ds.fact_sources.push(FactSource {
                fact_id,
                document_id: lo + self.rng.intn((hi - lo) as usize) as u32,
                quote,
            });
        }
    }

    fn generate_entity_sources(&mut self, ds: &mut Dataset) {
        let doc_range = doc_ranges_by_domain(ds);
        let entries: Vec<(u32, u32, u32)> = ds
            .entities
            .iter()
            .map(|e| {
                let &(lo, hi) = doc_range.get(e.domain.as_str()).unwrap_or(&(1, 2));
                (e.id, lo, hi)
            })
            .collect();
        drop(doc_range);
        for (entity_id, lo, hi) in entries {
            let n_links = self.rng.intn(2) + 1;
            let mut seen: HashMap<u32, bool> = HashMap::new();
            for _ in 0..n_links {
                let doc_id = lo + self.rng.intn((hi - lo) as usize) as u32;
                if seen.contains_key(&doc_id) {
                    continue;
                }
                seen.insert(doc_id, true);
                ds.entity_sources.push(EntitySource {
                    entity_id,
                    document_id: doc_id,
                });
            }
        }
    }

    fn generate_entity_links(&mut self, ds: &mut Dataset) -> Result<(), String> {
        let target = ds.scale.entities / 25;
        let target = if target == 0 && ds.entities.len() >= 10 {
            1
        } else {
            target
        };
        let entity_range = id_ranges_by_domain(&ds.entities, |e| &e.domain);
        let methods = ["rule", "equals", "llm"];
        let mut used_pairs: HashMap<(u32, u32), bool> = HashMap::new();

        for i in 0..target {
            let dom_a = DOMAINS[i % DOMAINS.len()];
            let dom_b = DOMAINS[(i + 1) % DOMAINS.len()];
            if dom_a == dom_b {
                continue;
            }
            let &(a_lo, a_hi) = entity_range.get(dom_a).ok_or("range a")?;
            let &(b_lo, b_hi) = entity_range.get(dom_b).ok_or("range b")?;

            let mut found = None;
            'outer: for _ in 0..64 {
                let s = a_lo + self.rng.intn((a_hi - a_lo) as usize) as u32;
                let t = b_lo + self.rng.intn((b_hi - b_lo) as usize) as u32;
                if s != t && !used_pairs.contains_key(&(s, t)) {
                    found = Some((s, t));
                    break 'outer;
                }
            }
            let Some((subject_id, target_id)) = found else {
                continue;
            };
            used_pairs.insert((subject_id, target_id), true);
            ds.entity_links.push(EntityLink {
                subject_id,
                target_id,
                relation_type: "same_entity".to_owned(),
                method: methods[self.rng.intn(methods.len())].to_owned(),
                confidence: round4(0.7 + self.rng.float64() * 0.29),
                evidence: format!(
                    "Cross-domain match between {dom_a:?} and {dom_b:?} generated by the benchmark loader."
                ),
            });
        }
        Ok(())
    }

    fn generate_chunk_entities(&mut self, ds: &mut Dataset) {
        let entity_range = id_ranges_by_domain(&ds.entities, |e| &e.domain);
        let doc_domain: HashMap<u32, &str> = ds
            .documents
            .iter()
            .map(|d| (d.id, d.domain.as_str()))
            .collect();

        for chunk in &ds.chunks {
            let domain = doc_domain.get(&chunk.doc_id).copied().unwrap_or("hr");
            let Some(&(lo, hi)) = entity_range.get(domain) else {
                continue;
            };
            if lo == hi {
                continue;
            }
            let count = self.rng.intn(3) + 1;
            let mut picked: HashMap<u32, bool> = HashMap::new();
            for _ in 0..count {
                let entity_id = lo + self.rng.intn((hi - lo) as usize) as u32;
                if picked.contains_key(&entity_id) {
                    continue;
                }
                picked.insert(entity_id, true);
                ds.chunk_entities.push(ChunkEntity {
                    chunk_id: chunk.id,
                    entity_id,
                });
            }
        }
    }

    fn build_samples(&mut self, ds: &mut Dataset) {
        let n = DEFAULT_SAMPLES_SIZE;
        ds.samples.doc_ids = sample_ids(&mut self.rng, ds.documents.len(), n);
        ds.samples.chunk_ids = sample_ids(&mut self.rng, ds.chunks.len(), n);
        ds.samples.fact_ids = sample_ids(&mut self.rng, ds.facts.len(), n);

        let mut candidates: Vec<u32> = Vec::new();
        let mut seen: HashMap<u32, bool> = HashMap::new();
        for f in &ds.facts {
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(f.subject_id) {
                e.insert(true);
                candidates.push(f.subject_id);
            }
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(f.object_id) {
                e.insert(true);
                candidates.push(f.object_id);
            }
        }
        for l in &ds.entity_links {
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(l.subject_id) {
                e.insert(true);
                candidates.push(l.subject_id);
            }
            if let std::collections::hash_map::Entry::Vacant(e) = seen.entry(l.target_id) {
                e.insert(true);
                candidates.push(l.target_id);
            }
        }
        candidates.sort();
        if candidates.len() < n && !ds.entities.is_empty() {
            for e in &ds.entities {
                if !seen.contains_key(&e.id) {
                    candidates.push(e.id);
                    if candidates.len() >= n * 2 {
                        break;
                    }
                }
            }
            candidates.sort();
        }
        ds.samples.entity_ids = shuffle_sample(&mut self.rng, &candidates, n);
        ds.samples.queries = self.generate_queries(ds);

        let mut types: Vec<String> = Vec::new();
        let mut seen_types: HashMap<&str, bool> = HashMap::new();
        for e in &ds.entities {
            if !seen_types.contains_key(e.entity_type.as_str()) {
                seen_types.insert(e.entity_type.as_str(), true);
                types.push(e.entity_type.clone());
            }
        }
        ds.samples.entity_types = types;
    }

    fn generate_queries(&mut self, ds: &Dataset) -> Vec<String> {
        let mut queries = Vec::new();
        for _ in 0..DEFAULT_SAMPLES_SIZE {
            let domain = DOMAINS[self.rng.intn(DOMAINS.len())];
            let vocab = domain_vocabulary(domain);
            let raw = match self.rng.intn(4) {
                0 => format!(
                    "{} {}",
                    vocab[self.rng.intn(vocab.len())],
                    vocab[self.rng.intn(vocab.len())]
                ),
                1 => vocab[self.rng.intn(vocab.len())].to_owned(),
                2 => {
                    if ds.entities.is_empty() {
                        vocab[0].to_owned()
                    } else {
                        ds.entities[self.rng.intn(ds.entities.len())].name.clone()
                    }
                }
                _ => format!(
                    "{} procedure policy review",
                    vocab[self.rng.intn(vocab.len())]
                ),
            };
            let safe = fts_safe_query(&raw);
            queries.push(if safe.is_empty() {
                "review".to_owned()
            } else {
                safe
            });
        }
        queries
    }

    fn chunk_text(&mut self, domain: &str) -> String {
        let vocab = domain_vocabulary(domain);
        let templates = [
            "The %s process must be documented and reviewed by the responsible team before approval.",
            "According to the current policy, %s applies to all departments starting from the next quarter.",
            "Each request related to %s is tracked in the central system until full resolution.",
            "The team is required to complete a training module covering %s within thirty days.",
            "%s remains a priority area, and progress is reported during regular status meetings.",
            "Exceptions for %s require written justification and sign-off from a manager.",
            "All records concerning %s are retained for at least five years in accordance with regulations.",
            "The procedure for handling %s was updated last month to reflect the new requirements.",
        ];
        let n = self.rng.intn(4) + 3;
        let mut sentences = Vec::with_capacity(n);
        for _ in 0..n {
            let tpl = templates[self.rng.intn(templates.len())];
            let word = vocab[self.rng.intn(vocab.len())];
            sentences.push(tpl.replace("%s", word));
        }
        sentences.join(" ")
    }
}

fn entity_types_by_domain(domain: &str) -> &'static [&'static str] {
    match domain {
        "hr" => &["employee", "department", "policy"],
        "product" => &["feature", "release", "requirement"],
        "engineering" => &["system", "service", "api"],
        "finance" => &["account", "budget", "vendor"],
        "security" => &["vulnerability", "certificate", "audit_rule"],
        _ => &["entity"],
    }
}

fn predicates_by_domain(domain: &str) -> &'static [&'static str] {
    match domain {
        "hr" => &[
            "works_in",
            "manages",
            "reports_to",
            "is_governed_by",
            "complies_with",
        ],
        "product" => &[
            "belongs_to",
            "depends_on",
            "blocks",
            "replaces",
            "satisfies",
        ],
        "engineering" => &[
            "calls",
            "hosts",
            "deploys",
            "integrates_with",
            "fails_over_to",
        ],
        "finance" => &[
            "charges_to",
            "budgeted_under",
            "invoiced_by",
            "approved_by",
            "settled_via",
        ],
        "security" => &[
            "affects",
            "mitigated_by",
            "detected_by",
            "audited_by",
            "scopes_access_to",
        ],
        _ => &["relates_to"],
    }
}

fn domain_vocabulary(domain: &str) -> &'static [&'static str] {
    match domain {
        "hr" => &[
            "hiring",
            "vacation",
            "severance",
            "onboarding",
            "performance review",
            "compensation",
            "benefits",
            "leave of absence",
            "termination notice",
            "probation period",
        ],
        "product" => &[
            "roadmap",
            "backlog",
            "feature flag",
            "release notes",
            "user story",
            "acceptance criteria",
            "milestone",
            "sprint planning",
            "stakeholder review",
            "deprecation plan",
        ],
        "engineering" => &[
            "deployment pipeline",
            "incident response",
            "runbook",
            "service level objective",
            "load testing",
            "rollback procedure",
            "observability stack",
            "capacity planning",
            "technical debt",
            "code review board",
        ],
        "finance" => &[
            "budget approval",
            "expense report",
            "quarterly close",
            "invoice reconciliation",
            "vendor contract",
            "forecasting model",
            "cost center",
            "payment terms",
            "audit trail",
            "reimbursement policy",
        ],
        "security" => &[
            "vulnerability scan",
            "access control list",
            "encryption standard",
            "incident response plan",
            "penetration test",
            "compliance audit",
            "certificate rotation",
            "data classification",
            "threat model",
            "zero trust architecture",
        ],
        _ => &["general"],
    }
}

fn id_ranges_by_domain<T>(
    items: &[T],
    domain_of: impl Fn(&T) -> &str,
) -> HashMap<&str, (u32, u32)> {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for item in items {
        *counts.entry(domain_of(item)).or_insert(0) += 1;
    }
    let mut ranges = HashMap::new();
    let mut pos = 0u32;
    for d in &DOMAINS {
        let count = counts.get(*d).copied().unwrap_or(0);
        let lo = pos + 1;
        ranges.insert(*d, (lo, lo + count));
        pos += count;
    }
    ranges
}

fn doc_ranges_by_domain(ds: &Dataset) -> HashMap<&str, (u32, u32)> {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for doc in &ds.documents {
        *counts.entry(doc.domain.as_str()).or_insert(0) += 1;
    }
    let mut ranges = HashMap::new();
    let mut pos = 0u32;
    for d in &DOMAINS {
        let count = counts.get(*d).copied().unwrap_or(0);
        let lo = pos + 1;
        ranges.insert(*d, (lo, lo + count));
        pos += count;
    }
    ranges
}

fn sample_ids(rng: &mut SplitMix64, count: usize, n: usize) -> Vec<u32> {
    let n = n.min(count);
    let mut ids: Vec<u32> = (1..=count as u32).collect();
    rng.shuffle(&mut ids);
    ids.truncate(n);
    ids
}

fn shuffle_sample(rng: &mut SplitMix64, src: &[u32], n: usize) -> Vec<u32> {
    let n = n.min(src.len());
    let mut cp = src.to_vec();
    rng.shuffle(&mut cp);
    cp.truncate(n);
    cp
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn round4(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

fn fts_safe_query(q: &str) -> String {
    let mut kept = Vec::new();
    for field in q.split_whitespace() {
        for part in field.split(|c: char| !c.is_alphanumeric()) {
            if !part.is_empty() && !part.chars().all(|c| c.is_ascii_digit()) {
                kept.push(part.to_owned());
            }
        }
    }
    kept.join(" ")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn generator_is_deterministic() {
        let mut g1 = Generator::new(42);
        let ds1 = g1.generate(&Scale::parse("small").unwrap()).unwrap();
        let mut g2 = Generator::new(42);
        let ds2 = g2.generate(&Scale::parse("small").unwrap()).unwrap();

        assert_eq!(ds1.documents.len(), ds2.documents.len());
        assert_eq!(ds1.chunks.len(), ds2.chunks.len());
        assert_eq!(ds1.entities.len(), ds2.entities.len());
        assert_eq!(ds1.facts.len(), ds2.facts.len());
        assert_eq!(
            ds1.documents[0].original_path,
            ds2.documents[0].original_path
        );
        assert_eq!(ds1.chunks[0].text, ds2.chunks[0].text);
        assert_eq!(ds1.entities[0].name, ds2.entities[0].name);
    }

    #[test]
    fn different_seeds_produce_different_data() {
        let mut g1 = Generator::new(42);
        let ds1 = g1.generate(&Scale::parse("small").unwrap()).unwrap();
        let mut g2 = Generator::new(99);
        let ds2 = g2.generate(&Scale::parse("small").unwrap()).unwrap();
        assert_ne!(
            ds1.documents[0].metadata_json, ds2.documents[0].metadata_json,
            "different seeds must produce different data"
        );
    }

    #[test]
    fn parse_scale_rejects_unknown() {
        assert!(Scale::parse("huge").is_err());
        assert!(Scale::parse("SMALL").is_ok());
        assert!(Scale::parse(" medium ").is_ok());
    }
}
