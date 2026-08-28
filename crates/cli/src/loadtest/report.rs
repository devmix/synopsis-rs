//! Report formatting (human-readable table + JSON).
//!
//! Oracle: `../synopsis/internal/benchmark/report.go`.

use std::collections::HashMap;

use serde::Serialize;

use super::generator::Scale;
use super::runner::ToolStats;

/// Fill phase summary.
#[derive(Debug, Clone, Serialize)]
pub struct FillSummary {
    /// Duration in ms.
    pub duration_ms: f64,
    /// Number of vectors embedded.
    pub vectors: usize,
    /// Row counts per table.
    pub tables: HashMap<String, i64>,
}

/// Graph load summary.
#[derive(Debug, Clone, Serialize)]
pub struct GraphSummary {
    /// Load duration in ms.
    pub load_ms: f64,
    /// Number of nodes.
    pub nodes: usize,
    /// Number of edges.
    pub edges: usize,
}

/// The full benchmark report.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// Scale used.
    pub scale: Scale,
    /// PRNG seed.
    pub seed: i64,
    /// Whether the DB was filled.
    pub filled: bool,
    /// Number of iterations.
    pub iterations: usize,
    /// Pages per paginated call.
    pub pages_per_call: usize,
    /// Fill summary (None if not filled).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fill: Option<FillSummary>,
    /// Graph summary.
    pub graph: GraphSummary,
    /// Per-tool statistics.
    pub tools: Vec<ToolStats>,
}

/// Prints the human-readable report (tabwriter-style).
pub fn print(report: &Report) {
    print!("{}", render(report));
}

/// Renders the human-readable report (tabwriter-style).
pub fn render(report: &Report) -> String {
    let mut out = String::new();

    // Header.
    out.push_str(&format!(
        "scale={} seed={} filled={} iterations={} pages_per_call={}\n",
        report.scale.name, report.seed, report.filled, report.iterations, report.pages_per_call
    ));

    // Fill line.
    if let Some(ref fill) = report.fill {
        out.push_str(&format!(
            "fill: {:.0}ms  vectors={}  tables: {} docs, {} chunks, {} entities, {} facts\n",
            fill.duration_ms,
            fill.vectors,
            fill.tables.get("documents").copied().unwrap_or(0),
            fill.tables.get("chunks").copied().unwrap_or(0),
            fill.tables.get("entities").copied().unwrap_or(0),
            fill.tables.get("facts").copied().unwrap_or(0),
        ));
    }

    // Graph line.
    out.push_str(&format!(
        "graph: {:.0}ms  nodes={}  edges={}\n\n",
        report.graph.load_ms, report.graph.nodes, report.graph.edges
    ));

    // Tool table.
    let header = vec![
        "TOOL".to_owned(),
        "CALLS".to_owned(),
        "PAGES".to_owned(),
        "ERRORS".to_owned(),
        "AVG_MS".to_owned(),
        "P50_MS".to_owned(),
        "P95_MS".to_owned(),
        "P99_MS".to_owned(),
        "MAX_MS".to_owned(),
        "QPS".to_owned(),
    ];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(report.tools.len());
    for t in &report.tools {
        rows.push(vec![
            t.name.clone(),
            t.calls.to_string(),
            t.pages
                .map(|p| p.to_string())
                .unwrap_or_else(|| "\u{2014}".to_owned()),
            t.errors.to_string(),
            format!("{:.3}", t.avg_ms),
            format!("{:.3}", t.p50_ms),
            format!("{:.3}", t.p95_ms),
            format!("{:.3}", t.p99_ms),
            format!("{:.3}", t.max_ms),
            format!("{:.2}", t.throughput_qps),
        ]);
    }

    out.push_str(&tabulate(&header, &rows));
    out.push('\n');

    // First errors.
    for t in &report.tools {
        if let Some(ref err) = t.first_error {
            out.push_str(&format!("  [{}] {err}\n", t.name));
        }
    }

    out
}

/// Emulates Go's `text/tabwriter`: min width 2, padding 2, right-aligned columns.
fn tabulate(header: &[String], rows: &[Vec<String>]) -> String {
    let ncols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.len().max(2)).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(ncols) {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let mut out = String::new();
    let write_row = |cells: &[String]| {
        let mut line = String::new();
        for (i, (cell, width)) in cells.iter().zip(widths.iter()).enumerate() {
            let cell = cell.as_str();
            if i == ncols - 1 {
                line.push_str(cell);
            } else {
                line.push_str(&format!("{cell:0$}  ", width));
            }
        }
        line
    };

    out.push_str(&write_row(header));
    out.push('\n');
    for row in rows {
        out.push_str(&write_row(row));
        out.push('\n');
    }
    out
}

/// Serializes the report to a JSON string.
pub fn to_json(report: &Report) -> Result<String, String> {
    serde_json::to_string_pretty(report).map_err(|e| e.to_string())
}

/// Writes the JSON report to a file.
pub fn write_json(report: &Report, path: &std::path::Path) -> Result<(), String> {
    let json = to_json(report)?;
    std::fs::write(path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn tabulate_produces_aligned_output() {
        let header = vec!["A".to_owned(), "BB".to_owned(), "CCC".to_owned()];
        let rows = vec![vec!["1".to_owned(), "22".to_owned(), "333".to_owned()]];
        let out = tabulate(&header, &rows);
        // Column widths: [2, 2, 3]; padding is 2 spaces between columns.
        assert!(out.contains("A   BB  CCC"));
        assert!(out.contains("1   22  333"));
    }

    /// A deterministic sample report (one tool row, max_ms = 10.0).
    fn sample_report() -> Report {
        Report {
            scale: Scale::parse("small").unwrap(),
            seed: 42,
            filled: true,
            iterations: 100,
            pages_per_call: 3,
            fill: Some(FillSummary {
                duration_ms: 123.4,
                vectors: 10000,
                tables: HashMap::new(),
            }),
            graph: GraphSummary {
                load_ms: 5.6,
                nodes: 2000,
                edges: 5000,
            },
            tools: vec![ToolStats {
                name: "search".to_owned(),
                calls: 100,
                pages: None,
                errors: 0,
                first_error: None,
                total_ms: 500.0,
                attempt_ms: 490.0,
                avg_ms: 4.9,
                min_ms: 2.0,
                max_ms: 10.0,
                p50_ms: 4.5,
                p95_ms: 8.0,
                p99_ms: 10.0,
                throughput_qps: 200.0,
            }],
        }
    }

    #[test]
    fn report_serializes_to_json() {
        let report = sample_report();
        let json = to_json(&report).unwrap();
        assert!(json.contains("\"scale\""));
        assert!(json.contains("\"seed\": 42"));
        assert!(json.contains("\"search\""));
        // The JSON stays consistent with the table: max_ms is present.
        assert!(json.contains("\"max_ms\": 10.0"));
    }

    #[test]
    fn report_table_contains_max_ms_column() {
        let out = render(&sample_report());
        let header = out
            .lines()
            .find(|line| line.starts_with("TOOL"))
            .expect("header line");
        // Column order per the task body: CALLS/AVG/P50/P95/P99/MAX ms/QPS.
        let calls = header.find("CALLS").expect("CALLS");
        let avg = header.find("AVG_MS").expect("AVG_MS");
        let p99 = header.find("P99_MS").expect("P99_MS");
        let max = header.find("MAX_MS").expect("MAX_MS");
        let qps = header.find("QPS").expect("QPS");
        assert!(calls < avg, "AVG must follow CALLS");
        assert!(p99 < max, "MAX_MS must follow P99_MS");
        assert!(max < qps, "MAX_MS must precede QPS");
        // The per-tool row carries the max_ms value.
        let row = out
            .lines()
            .find(|line| line.starts_with("search"))
            .expect("search row");
        assert!(row.contains("10.000"), "row must carry max_ms: {row}");
    }
}
