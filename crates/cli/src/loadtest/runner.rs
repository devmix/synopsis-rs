//! Benchmark runner: drives `mcp::Server::dispatch` for N iterations.
//!
//! Note: `search_lexical` and `search_semantic` are not MCP tools, so they
//! are not benchmarked.

use std::time::Instant;

use serde::Serialize;
use serde_json::json;

use mcp::Server;

use super::generator::{DEFAULT_SAMPLES_SIZE, Samples};

/// Static fallback queries used when `--no-fill` is set.
pub const STATIC_NO_FILL_QUERIES: [&str; 16] = [
    "hiring process policy review",
    "vacation leave of absence procedure",
    "roadmap milestone sprint planning",
    "deployment pipeline rollback procedure",
    "incident response runbook observability",
    "budget approval quarterly close forecast",
    "expense report invoice reconciliation",
    "vulnerability scan penetration test audit",
    "access control list encryption standard",
    "feature flag release notes user story",
    "service level objective capacity planning",
    "vendor contract payment terms cost center",
    "performance review compensation benefits",
    "code review board technical debt runbook",
    "compliance audit data classification threat",
    "deprecation plan stakeholder review backlog",
];

/// Per-tool benchmark statistics.
#[derive(Debug, Clone, Serialize)]
pub struct ToolStats {
    /// Tool name.
    pub name: String,
    /// Number of calls made.
    pub calls: usize,
    /// Number of pages fetched (paginated tools only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pages: Option<usize>,
    /// Number of errors.
    pub errors: usize,
    /// First error message (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_error: Option<String>,
    /// Total wall-clock time in ms.
    pub total_ms: f64,
    /// Sum of individual attempt times in ms.
    pub attempt_ms: f64,
    /// Mean attempt time in ms.
    pub avg_ms: f64,
    /// Minimum attempt time in ms.
    pub min_ms: f64,
    /// Maximum attempt time in ms.
    pub max_ms: f64,
    /// 50th percentile in ms.
    pub p50_ms: f64,
    /// 95th percentile in ms.
    pub p95_ms: f64,
    /// 99th percentile in ms.
    pub p99_ms: f64,
    /// Throughput in queries per second.
    pub throughput_qps: f64,
}

/// Configuration for a benchmark run.
pub struct Options {
    /// Number of iterations.
    pub iterations: usize,
    /// Pages to fetch per paginated call.
    pub pages_per_call: usize,
}

/// The benchmark runner.
pub struct Runner {
    server: std::sync::Arc<Server>,
    samples: Samples,
    iterations: usize,
    pages_per_call: usize,
}

impl Runner {
    /// Creates a runner.
    pub fn new(server: std::sync::Arc<Server>, samples: Samples) -> Self {
        Self {
            server,
            samples,
            iterations: 100,
            pages_per_call: 3,
        }
    }

    /// Sets iterations.
    pub fn with_iterations(mut self, n: usize) -> Self {
        self.iterations = n;
        self
    }

    /// Sets pages per call.
    pub fn with_pages_per_call(mut self, n: usize) -> Self {
        self.pages_per_call = n;
        self
    }

    /// Runs the benchmark and returns per-tool stats.
    pub fn run(&self) -> Vec<ToolStats> {
        vec![
            self.run_search(),
            self.run_no_args("catalog_overview"),
            self.run_paginated("catalog_documents", json!({})),
            self.run_paginated("catalog_entities", json!({})),
            self.run_search_entities_by_type(),
            self.run_search_facts(),
            self.run_get_document_context(),
            self.run_get_chunk_by_id(),
            self.run_get_fact_by_id(),
            self.run_get_entity_dossier(),
            self.run_get_entity_relations(),
            self.run_get_entity_links(),
        ]
    }

    fn run_search(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations);
        let mut errors = 0usize;
        let mut first_error = None;
        let start = Instant::now();

        for i in 0..self.iterations {
            let query = self
                .samples
                .queries
                .get(i % self.samples.queries.len())
                .cloned()
                .unwrap_or_else(|| {
                    STATIC_NO_FILL_QUERIES[i % STATIC_NO_FILL_QUERIES.len()].to_owned()
                });
            let args = json!({ "query": query, "top_k": 10 });
            let t0 = Instant::now();
            match self.server.dispatch("search", Some(&args)) {
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
            attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "search",
            self.iterations,
            None,
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_no_args(&self, name: &str) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations);
        let mut errors = 0usize;
        let mut first_error = None;
        let start = Instant::now();

        for _ in 0..self.iterations {
            let t0 = Instant::now();
            match self.server.dispatch(name, None) {
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
            attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            name,
            self.iterations,
            None,
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_paginated(&self, name: &str, base_args: serde_json::Value) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations * self.pages_per_call);
        let mut errors = 0usize;
        let mut first_error = None;
        let mut pages = 0usize;
        let start = Instant::now();

        for _ in 0..self.iterations {
            let mut cursor: Option<String> = None;
            for _ in 0..self.pages_per_call {
                let mut args = base_args.clone();
                if let Some(c) = &cursor
                    && let Some(obj) = args.as_object_mut()
                {
                    obj.insert("cursor".to_owned(), json!(c));
                }
                let t0 = Instant::now();
                match self.server.dispatch(name, Some(&args)) {
                    Ok(resp) => {
                        pages += 1;
                        if let Some(c) = resp.get("cursor").and_then(|v| v.as_str()) {
                            cursor = Some(c.to_owned());
                        } else {
                            break;
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        if first_error.is_none() {
                            first_error = Some(e.to_string());
                        }
                        break;
                    }
                }
                attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            name,
            self.iterations,
            Some(pages),
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_search_entities_by_type(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations * self.pages_per_call);
        let mut errors = 0usize;
        let mut first_error = None;
        let mut pages = 0usize;
        let start = Instant::now();

        for i in 0..self.iterations {
            let etype = self
                .samples
                .entity_types
                .get(i % self.samples.entity_types.len())
                .cloned()
                .unwrap_or_else(|| "employee".to_owned());
            let mut cursor: Option<String> = None;
            for _ in 0..self.pages_per_call {
                let mut args = json!({ "entity_type": etype });
                if let Some(c) = &cursor
                    && let Some(obj) = args.as_object_mut()
                {
                    obj.insert("cursor".to_owned(), json!(c));
                }
                let t0 = Instant::now();
                match self.server.dispatch("search_entities_by_type", Some(&args)) {
                    Ok(resp) => {
                        pages += 1;
                        if let Some(c) = resp.get("cursor").and_then(|v| v.as_str()) {
                            cursor = Some(c.to_owned());
                        } else {
                            break;
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        if first_error.is_none() {
                            first_error = Some(e.to_string());
                        }
                        break;
                    }
                }
                attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "search_entities_by_type",
            self.iterations,
            Some(pages),
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_search_facts(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations * self.pages_per_call);
        let mut errors = 0usize;
        let mut first_error = None;
        let mut pages = 0usize;
        let start = Instant::now();

        for _ in 0..self.iterations {
            let mut cursor: Option<String> = None;
            for _ in 0..self.pages_per_call {
                let mut args = json!({});
                if let Some(c) = &cursor
                    && let Some(obj) = args.as_object_mut()
                {
                    obj.insert("cursor".to_owned(), json!(c));
                }
                let t0 = Instant::now();
                match self.server.dispatch("search_facts", Some(&args)) {
                    Ok(resp) => {
                        pages += 1;
                        if let Some(c) = resp.get("cursor").and_then(|v| v.as_str()) {
                            cursor = Some(c.to_owned());
                        } else {
                            break;
                        }
                    }
                    Err(e) => {
                        errors += 1;
                        if first_error.is_none() {
                            first_error = Some(e.to_string());
                        }
                        break;
                    }
                }
                attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
            }
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "search_facts",
            self.iterations,
            Some(pages),
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_get_document_context(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations);
        let mut errors = 0usize;
        let mut first_error = None;
        let start = Instant::now();

        for i in 0..self.iterations {
            let doc_id = self
                .samples
                .doc_ids
                .get(i % self.samples.doc_ids.len())
                .copied()
                .unwrap_or(1);
            let args = json!({ "document_id": doc_id.to_string(), "include_chunks": true, "include_entities": true, "include_facts": true });
            let t0 = Instant::now();
            match self.server.dispatch("get_document_context", Some(&args)) {
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
            attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "get_document_context",
            self.iterations,
            None,
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_get_chunk_by_id(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations);
        let mut errors = 0usize;
        let mut first_error = None;
        let start = Instant::now();

        for i in 0..self.iterations {
            let id = self
                .samples
                .chunk_ids
                .get(i % self.samples.chunk_ids.len())
                .copied()
                .unwrap_or(1);
            let args = json!({ "chunk_id": id.to_string() });
            let t0 = Instant::now();
            match self.server.dispatch("get_chunk_by_id", Some(&args)) {
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
            attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "get_chunk_by_id",
            self.iterations,
            None,
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_get_fact_by_id(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations);
        let mut errors = 0usize;
        let mut first_error = None;
        let start = Instant::now();

        for i in 0..self.iterations {
            let id = self
                .samples
                .fact_ids
                .get(i % self.samples.fact_ids.len())
                .copied()
                .unwrap_or(1);
            let args = json!({ "fact_id": id.to_string() });
            let t0 = Instant::now();
            match self.server.dispatch("get_fact_by_id", Some(&args)) {
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
            attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "get_fact_by_id",
            self.iterations,
            None,
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_get_entity_dossier(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations);
        let mut errors = 0usize;
        let mut first_error = None;
        let start = Instant::now();

        for i in 0..self.iterations {
            let id = self
                .samples
                .entity_ids
                .get(i % self.samples.entity_ids.len())
                .copied()
                .unwrap_or(1);
            let args = json!({ "entity_id": id.to_string() });
            let t0 = Instant::now();
            match self.server.dispatch("get_entity_dossier", Some(&args)) {
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
            attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "get_entity_dossier",
            self.iterations,
            None,
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_get_entity_relations(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations);
        let mut errors = 0usize;
        let mut first_error = None;
        let start = Instant::now();

        for i in 0..self.iterations {
            let id = self
                .samples
                .entity_ids
                .get(i % self.samples.entity_ids.len())
                .copied()
                .unwrap_or(1);
            let args = json!({ "entity_id": id.to_string(), "include_cross_domain": true });
            let t0 = Instant::now();
            match self.server.dispatch("get_entity_relations", Some(&args)) {
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
            attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "get_entity_relations",
            self.iterations,
            None,
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }

    fn run_get_entity_links(&self) -> ToolStats {
        let mut attempts = Vec::with_capacity(self.iterations);
        let mut errors = 0usize;
        let mut first_error = None;
        let start = Instant::now();

        for i in 0..self.iterations {
            let id = self
                .samples
                .entity_ids
                .get(i % self.samples.entity_ids.len())
                .copied()
                .unwrap_or(1);
            let args = json!({ "entity_id": id.to_string() });
            let t0 = Instant::now();
            match self.server.dispatch("get_entity_links", Some(&args)) {
                Ok(_) => {}
                Err(e) => {
                    errors += 1;
                    if first_error.is_none() {
                        first_error = Some(e.to_string());
                    }
                }
            }
            attempts.push(t0.elapsed().as_secs_f64() * 1000.0);
        }

        let total_ms = start.elapsed().as_secs_f64() * 1000.0;
        make_stats(
            "get_entity_links",
            self.iterations,
            None,
            errors,
            first_error,
            total_ms,
            attempts,
        )
    }
}

/// Loads benchmark samples from the database after fill.
pub fn load_samples_from_db(_db: &db::Db, ds: &super::generator::Dataset) -> Samples {
    // Samples come from the generator (deterministic); the DB is available
    // for future validation or re-sampling.
    ds.samples.clone()
}

/// Static samples for `--no-fill` mode (no DB access needed).
pub fn load_samples_static() -> Samples {
    Samples {
        queries: STATIC_NO_FILL_QUERIES
            .iter()
            .map(|s| s.to_string())
            .collect(),
        doc_ids: (1..=DEFAULT_SAMPLES_SIZE as u32).collect(),
        chunk_ids: (1..=DEFAULT_SAMPLES_SIZE as u32).collect(),
        fact_ids: (1..=DEFAULT_SAMPLES_SIZE as u32).collect(),
        entity_ids: (1..=DEFAULT_SAMPLES_SIZE as u32).collect(),
        entity_types: vec![
            "employee".to_owned(),
            "system".to_owned(),
            "feature".to_owned(),
            "account".to_owned(),
            "vulnerability".to_owned(),
        ],
    }
}

/// Builds ToolStats from raw attempt timings.
fn make_stats(
    name: &str,
    calls: usize,
    pages: Option<usize>,
    errors: usize,
    first_error: Option<String>,
    total_ms: f64,
    attempts: Vec<f64>,
) -> ToolStats {
    let attempt_ms: f64 = attempts.iter().sum();
    let n = attempts.len().max(1);
    let avg_ms = attempt_ms / n as f64;
    let min_ms = attempts.iter().cloned().fold(f64::MAX, f64::min);
    let max_ms = attempts.iter().cloned().fold(0.0, f64::max);
    let (p50, p95, p99) = percentiles(&attempts);
    let throughput_qps = if total_ms > 0.0 {
        (calls as f64) / (total_ms / 1000.0)
    } else {
        0.0
    };

    ToolStats {
        name: name.to_owned(),
        calls,
        pages,
        errors,
        first_error,
        total_ms,
        attempt_ms,
        avg_ms,
        min_ms: if attempts.is_empty() { 0.0 } else { min_ms },
        max_ms,
        p50_ms: p50,
        p95_ms: p95,
        p99_ms: p99,
        throughput_qps,
    }
}

/// Computes p50, p95, p99 from a slice of timings.
fn percentiles(data: &[f64]) -> (f64, f64, f64) {
    if data.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut sorted = data.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (
        percentile_at(&sorted, 50.0),
        percentile_at(&sorted, 95.0),
        percentile_at(&sorted, 99.0),
    )
}

fn percentile_at(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (p / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}
