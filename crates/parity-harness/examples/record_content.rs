//! One-time recorder for the content-parity golden fixtures (task 1.3).
//!
//! Drives the Go oracle (`../synopsis/bin/synopsis`) over the content corpus
//! (task 1.2) to record the four golden fixtures: `search`, `catalog_overview`,
//! `catalog_documents`, and `catalog_entities`. Each fixture is committed to
//! `fixtures/content/<tool>.json` (the oracle's exact response payload,
//! pretty-printed, keys sorted) and a self-describing `fixtures/content/README.md`
//! records the corpus, the Go config, and the exact tool args.
//!
//! # Flow
//!
//! 1. Write the content corpus + a temporary Go config + `global.xml` to a
//!    scratch dir under `target/parity-go/`, and copy the pre-installed ONNX
//!    runtime into the scratch data dir (fully offline).
//! 2. Run `synopsis sync` to ingest the corpus.
//! 3. Run `synopsis serve` on a free loopback port (legacy SSE transport).
//! 4. Drive the four tools and commit each response to a fixture.
//! 5. Shut the server down and write the `README.md` header.
//!
//! # Transport note (deviation from the task body)
//!
//! The task body says to point `content_parity::record_response` at
//! `http://127.0.0.1:<port>/mcp`. The Go oracle (mcp-go v0.57.0
//! `NewSSEServer`) serves the **legacy SSE** transport only — `GET /sse` +
//! `POST /message`; `POST /mcp` answers `404`. `record_response` uses the
//! Streamable-HTTP `McpClient`, which cannot speak to the Go server. This
//! driver therefore drives the oracle with the harness's [`SseClient`]
//! (built exactly for this wire contract) and writes the fixture bytes with
//! the same semantics `record_response` uses (raw payload, pretty, sorted
//! keys, one trailing newline).
//!
//! # Run
//!
//! ```sh
//! cargo run -p parity-harness --example record_content
//! ```
//!
//! Requires the Go binary + bge-small model + ONNX runtime locally. Aborts
//! with a clear error if any are missing — it never fabricates fixtures.
//! It is an example, so it is not run by the default `cargo test` gate.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use parity_harness::corpus::write_content_corpus;
use parity_harness::sse_client::SseClient;
use serde_json::{Value, json};

/// Fixed `search` query + `top_k` (documented in the fixture `README.md`).
/// Chosen to span the `product` and `eng` domains (cross-domain signal).
const SEARCH_QUERY: &str = "Atlas dashboard builder";
const SEARCH_TOP_K: u32 = 5;
/// Fixed `catalog_documents` page size — below the 8-doc total, so the
/// paginated page + `next_cursor` are exercised.
const CATALOG_PAGE_SIZE: u32 = 3;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let repo_root = repo_root()?;
    let go_root = repo_root.join("..").join("synopsis");
    let go_bin = go_root.join("bin").join("synopsis");
    let model = repo_root.join("workspace/models/bge-small-en-v1.5/model.onnx");
    let onnx_lib = repo_root.join("workspace/onnxruntime/libonnxruntime.so.1.28.0");
    let fixtures_dir = repo_root.join("crates/parity-harness/fixtures/content");

    // Refuse to fabricate: the Go binary + model + runtime must all be present.
    for (label, path) in [
        ("Go oracle binary", go_bin.as_path()),
        ("bge-small model", model.as_path()),
        ("ONNX runtime", onnx_lib.as_path()),
    ] {
        if !path.is_file() {
            return Err(format!(
                "{label} not found at {} — cannot record (refusing to fabricate fixtures)",
                path.display()
            )
            .into());
        }
    }

    let scratch = repo_root.join("target/parity-go");
    let port = free_port()?;
    setup_scratch(&scratch, &go_root, &model, &onnx_lib, port)?;
    println!(
        "[1/5] scratch env ready at {} (port {port})",
        scratch.display()
    );

    run_sync(&scratch, &go_bin)?;
    println!("[2/5] Go `sync` ingested the corpus");

    let server = ServeGuard(start_serve(&scratch, &go_bin)?);
    let base_url = format!("http://127.0.0.1:{port}");
    wait_for_searcher_ready(&base_url).await?;
    println!("[3/5] Go server ready at {base_url}");

    let mut client = SseClient::connect(&base_url).await?;
    client.initialize().await?;
    let _tools = client.list_tools().await?;

    for (tool, args, file) in [
        (
            "search",
            json!({ "query": SEARCH_QUERY, "top_k": SEARCH_TOP_K }),
            "search.json",
        ),
        ("catalog_overview", json!({}), "catalog_overview.json"),
        (
            "catalog_documents",
            json!({ "page_size": CATALOG_PAGE_SIZE }),
            "catalog_documents.json",
        ),
        ("catalog_entities", json!({}), "catalog_entities.json"),
    ] {
        let result = client.call_tool(tool, args).await?;
        let payload = payload_from_call_result(tool, &result)?;
        write_fixture(&fixtures_dir.join(file), &payload)?;
        println!("[4/5] recorded {file}");
    }

    write_readme(&fixtures_dir)?;
    println!("[5/5] wrote fixtures/content/README.md");

    // Shut the server down explicitly (the guard also covers early `?` returns).
    drop(server);
    println!("done — fixtures written to {}", fixtures_dir.display());
    Ok(())
}

/// Owns the Go `serve` child process and kills it on drop, so a mid-recording
/// failure (propagated via `?`) does not leave an orphaned server behind.
struct ServeGuard(Child);

impl Drop for ServeGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Repo root from the example's manifest dir (`crates/parity-harness` → two up).
fn repo_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = manifest
        .parent()
        .and_then(Path::parent)
        .ok_or("cannot resolve the repo root from CARGO_MANIFEST_DIR")?;
    Ok(root.to_path_buf())
}

/// Bind a loopback listener to port 0 to obtain a free port, then release it.
fn free_port() -> Result<u16, Box<dyn std::error::Error>> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

/// Build the scratch env from scratch: corpus + Go config + `global.xml` + the
/// offline ONNX runtime copy. Idempotent (the scratch dir is removed first).
fn setup_scratch(
    scratch: &Path,
    go_root: &Path,
    model: &Path,
    onnx_lib: &Path,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let _ = std::fs::remove_dir_all(scratch);
    let documents = scratch.join("documents");
    let ontology = scratch.join("ontology");
    let data = scratch.join("data");
    std::fs::create_dir_all(&documents)?;
    std::fs::create_dir_all(&ontology)?;
    std::fs::create_dir_all(data.join("onnxruntime"))?;

    // The content corpus (task 1.2): 8 docs under hr/ product/ eng/.
    write_content_corpus(&documents)?;

    // Offline ONNX runtime: copy the pre-installed library + write the cache
    // manifest the Go `LibraryManager.IsInstalled` check reads.
    let lib_dst = data.join("onnxruntime/libonnxruntime.so.1.28.0");
    std::fs::copy(onnx_lib, &lib_dst)?;
    let cache = format!(
        "{{\n  \"version\": \"1.28.0\",\n  \"library_path\": \"{}\",\n  \
         \"install_time\": \"2026-09-02T00:00:00Z\",\n  \"platform\": \
         \"linux-amd64\"\n}}\n",
        lib_dst.display()
    );
    std::fs::write(data.join("onnxruntime/.cache.json"), cache)?;

    // The temporary Go config (matches the Rust parity harness).
    let config = format!(
        r#"database:
  path: {db}
embeddings:
  mode: local
  local:
    model_name: bge-small-en-v1.5
    model_path: {model}
    vector_dim: 384
ingestion:
  ner:
    disabled: true
  chunking:
    markdown:
      strategy: hybrid
      max_chunk_size: 8192
      overlap_size: 100
      min_section_size: 500
paths:
  data_dir: {data}
  documents_dir: {documents}
  migrations_dir: {migrations}
  global_config_path: {ontology}
  prompts_path: {prompts}
  onnx_config: {onnx}
server:
  name: synopsis
  host: 127.0.0.1
  port: {port}
search:
  rrf_k: 20
  lexical_top_k: 20
  semantic_top_k: 20
  final_top_k: 10
  enable_lexical: true
  enable_semantic: true
  timeout_ms: 10000
graph:
  enable_graph: false
linker:
  disabled: true
auto_update:
  enabled: false
  initial_sync: false
  watch_sources: false
logging:
  level: info
"#,
        db = data.join("knowledge.db").display(),
        model = model.display(),
        data = data.display(),
        documents = documents.display(),
        migrations = go_root.join("migrations").display(),
        ontology = ontology.display(),
        prompts = go_root.join("configs/prompts").display(),
        onnx = go_root.join("configs/onnx.yaml").display(),
        port = port,
    );
    std::fs::write(scratch.join("parity.yaml"), config)?;

    // `global.xml`: three sources (one per domain sub-directory), each with its
    // own domain, so every document carries its sub-directory's domain.
    let global_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<global>
  <sources>
    <source path="{hr}" type="markdown">
      <domains><domain>hr</domain></domains>
    </source>
    <source path="{product}" type="markdown">
      <domains><domain>product</domain></domains>
    </source>
    <source path="{eng}" type="markdown">
      <domains><domain>eng</domain></domains>
    </source>
  </sources>
</global>
"#,
        hr = documents.join("hr").display(),
        product = documents.join("product").display(),
        eng = documents.join("eng").display(),
    );
    std::fs::write(ontology.join("global.xml"), global_xml)?;
    Ok(())
}

/// Run `synopsis sync` and require a successful ingestion summary.
fn run_sync(scratch: &Path, go_bin: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let config_arg = scratch.join("parity.yaml").to_string_lossy().into_owned();
    let output = Command::new(go_bin)
        .arg("-config")
        .arg(&config_arg)
        .arg("sync")
        .current_dir(scratch)
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}\n{stderr}");
    if !output.status.success() {
        return Err(format!(
            "Go `sync` failed ({}):\n--- stderr ---\n{stderr}\n--- stdout ---\n{stdout}",
            output.status
        )
        .into());
    }
    // The Go logger writes to stderr; the ingestion summary may land on either
    // stream, so search the combined output.
    if !combined.contains("Documents created") {
        return Err(format!("Go `sync` produced no ingestion summary:\n{combined}").into());
    }
    Ok(())
}

/// Start `synopsis serve` (the port comes from the scratch config).
fn start_serve(scratch: &Path, go_bin: &Path) -> Result<Child, Box<dyn std::error::Error>> {
    let config_arg = scratch.join("parity.yaml").to_string_lossy().into_owned();
    let log_file = std::fs::File::create(scratch.join("serve.log"))?;
    Ok(Command::new(go_bin)
        .arg("-config")
        .arg(&config_arg)
        .arg("serve")
        .current_dir(scratch)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file))
        .spawn()?)
}

/// Poll `/health` until the searcher component reports `ok` (the ONNX session
/// and vector index are loaded and the tools are ready to answer).
async fn wait_for_searcher_ready(base_url: &str) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("{base_url}/health");
    let http = reqwest::Client::new();
    for _ in 0..120 {
        if let Ok(response) = http.get(&url).send().await
            && response.status().is_success()
        {
            let body = response.text().await?;
            if body.contains("\"searcher\":\"ok\"") {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Err(format!("Go server searcher did not become ready within 30s at {url}").into())
}

/// The oracle-shaped JSON payload of a `tools/call` result: the first `text`
/// content block, parsed. A tool-level error result (`isError`) is a failure.
fn payload_from_call_result(
    tool: &str,
    result: &Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let text = text_of(result);
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(format!("tool `{tool}` returned an error result: {text}").into());
    }
    if text.is_empty() {
        return Err(format!("tool `{tool}` returned no text content").into());
    }
    serde_json::from_str(&text)
        .map_err(|err| format!("tool `{tool}` payload is not valid JSON: {err}").into())
}

/// The first `text` content block of a `CallToolResult` (empty string if none).
fn text_of(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .and_then(|block| block.get("text").and_then(Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

/// Write the payload as a committed fixture (pretty, keys sorted, one trailing
/// newline) — the same shape `content_parity::record_response` produces.
fn write_fixture(out: &Path, payload: &Value) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let rendered = serde_json::to_string_pretty(payload)?;
    std::fs::write(out, format!("{rendered}\n"))?;
    Ok(())
}

/// The self-describing header for the fixture set: corpus + Go config + the
/// exact tool args + the re-record command (task 1.3 acceptance #1).
fn write_readme(fixtures_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let readme = format!(
        r#"# Content-parity golden fixtures

Recorded once from the Go oracle (`../synopsis/bin/synopsis`) over the content
corpus (task 1.2). The content-parity tests (tasks 1.4/1.5) compare the Rust
tool responses against these files after `content_parity::normalize`. Each
`.json` file is the oracle's exact response payload (raw, pretty, keys
sorted); this file is the header that makes the set self-describing and
re-recordable.

## Corpus (task 1.2, `corpus::write_content_corpus`)

8 static markdown documents across 3 domains — `hr/` (3), `product/` (3),
`eng/` (2) — with a recurring entity vocabulary (Dana Kovac, Marcus Webb,
Priya Sharma; Atlas, Portal, Ledger, Beacon; Vacation Policy, Remote Work
Policy, API v2 deprecation). No randomness or timestamps.

## Go recording config (`target/parity-go/parity.yaml`)

- model: **bge-small-en-v1.5, 384-dim** (explicit `model_path`, offline —
  no download); ONNX runtime 1.28.0 copied from `workspace/onnxruntime`.
- NER: **disabled** (`ingestion.ner.disabled: true`) — no entities/facts.
- graph: **disabled** (`graph.enable_graph: false`); linker: **disabled**
  (`linker.disabled: true`).
- chunking (markdown): `hybrid`, max 8192, overlap 100, min section 500.
- search: `rrf_k 20`, lexical/semantic `top_k 20`, final `top_k 10`, both
  legs enabled, `timeout_ms 10000`.
- `global.xml`: 3 sources (one per domain sub-directory), each with its own
  domain, so each document carries its sub-directory's domain.

Note: the Go `ApplyDefaults` forces non-zero search boosts
(`deprecated 0.2`, `official 1.5`, `recent 1.2 / 90d`, `authority default 1.0`)
that the Rust harness leaves at zero. For this corpus they are neutral to
rank order (all docs share one ingestion timestamp → uniform recent boost;
no deprecated/official content; authority `1.0`), and `normalize` strips the
exact `score` — so only rank order (identity fields) is compared.

## Per-fixture tool args

| fixture | tool | args |
|---|---|---|
| `search.json` | `search` | `{{"query": "{query}", "top_k": {top_k}}}` |
| `catalog_overview.json` | `catalog_overview` | `{{}}` |
| `catalog_documents.json` | `catalog_documents` | `{{"page_size": {page_size}}}` |
| `catalog_entities.json` | `catalog_entities` | `{{}}` (empty — NER disabled) |

## Transport (deviation from the task body)

The Go oracle (mcp-go v0.57.0 `NewSSEServer`) serves the **legacy SSE**
transport only (`GET /sse` + `POST /message`); `POST /mcp` (Streamable HTTP)
answers `404`. `content_parity::record_response` uses the Streamable-HTTP
`McpClient`, so it cannot reach the Go server. The recorder
(`examples/record_content.rs`) drives the oracle with the harness `SseClient`
and writes the fixtures with `record_response`'s byte semantics.

## Re-record

```sh
cargo run -p parity-harness --example record_content
```
"#,
        query = SEARCH_QUERY,
        top_k = SEARCH_TOP_K,
        page_size = CATALOG_PAGE_SIZE,
    );
    std::fs::create_dir_all(fixtures_dir)?;
    std::fs::write(fixtures_dir.join("README.md"), readme)?;
    Ok(())
}
