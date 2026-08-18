//! S2 spike: ONNX Runtime seam via `ort` (change native-seam-spikes task 2.1; design D3/D5).
//!
//! Proves that Rust can load ORT 1.28 through the onnx.yaml mechanism and produce valid,
//! deterministic embeddings for EVERY model in the registry — not just one:
//!   - bge-m3-int8 (vector_dim=1024),
//!   - bge-small-en-v1.5 (vector_dim=384; `default` of both onnx.yaml and config.default.yaml),
//!   - paraphrase-multilingual-MiniLM-L12-v2 (vector_dim=384).
//!
//! Each model runs its own input/output pipeline exactly as the oracle detects it from the graph:
//! inputs are bound in canonical order `input_ids, attention_mask, token_type_ids` when declared
//! (BGE exports omit/keep token_type_ids differently), and the output is selected by the oracle's
//! `selectOutputName` (sentence_embedding → last_hidden_state → single declared output).
//!
//!   1. The model/runtime registry is transcribed from `../synopsis/configs/onnx.yaml` (the
//!      authoritative source) into constants below; at start-up the spike re-checks that the
//!      yaml file still contains every transcribed literal, so a stale transcription fails loud.
//!   2. Artifacts live in `data/` with the oracle's layout (`data/onnxruntime/libonnxruntime.so.*`,
//!      `data/models/<name>/*`); existence + declared `size_bytes` are verified per file for all
//!      three models, and the runtime `.so` must embed its version string (ORT 1.28.0). Artifacts
//!      were preloaded from the oracle's own already-downloaded copy — see data/README.md.
//!   3. The session is created through `ort::init_from(<abs path to .so>)` + dynamic loading
//!      (feature `load-dynamic`, API level 28), mirroring the oracle's SetSharedLibraryPath/dlopen.
//!   4. Embeddings replicate the oracle pipeline exactly (`../synopsis/internal/embedding/
//!      onnx_provider.go` + `sugarme_tokenizer.go`): encode(text, add_special_tokens=false),
//!      truncate to 512 tokens, right-pad with pad id 0 (the oracle's JSON-path quirk),
//!      attention_mask = all ones over the full padded length, then L2-normalize in f64 and cast
//!      back to f32.
//!   5. Per-model acceptance checks: 50 hardcoded texts -> registry-dim finite vectors; unit norm
//!      within eps; an in-process second inference pass must be bit-identical (design D5
//!      self-consistency); throughput ms/text and peak RSS are reported for the laptop budget.
//!      Cross-run bit-identity: run twice per model with `--model <m> --out <dir>` and diff the
//!      JSONL; single-model processes make their final VmPeak the true per-model peak RSS.
//!
//! Usage (from repo root):
//!   cargo run -p spikes --bin s2_onnx [--onnx-yaml <path>] [--data-dir <path>] \
//!       [--model <name>] [--out <dir>]
//! Without `--model` all three registry models are loaded and embedded sequentially in one process.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

/// Oracle `DefaultMaxLength` (../synopsis/internal/embedding/tokenizer.go).
const MAX_LENGTH: usize = 512;
/// Pad id used by the oracle on the tokenizer.json path: `SugarmeTokenizer.padID` keeps its zero
/// value there (`sugarme_tokenizer.go`, loadTokenizerJSON never sets it) — replicated verbatim.
const PAD_ID: i64 = 0;

// ── Registry transcription from ../synopsis/configs/onnx.yaml ────────────────────────
// The yaml is the authoritative source (frozen contract, config-format spec); these constants
// are re-checked against the file text at every start-up (check_registry_transcription).

/// runtime.version for all platforms.
const RUNTIME_VERSION: &str = "1.28.0";
/// runtime.platforms[linux-amd64].library_name — this spike runs on x86_64 Linux (laptop target);
/// the constant keeps a fixed name so the crate still compiles for the CI cross-build matrix, and
/// non-matching platforms fail at start-up with an artifact-missing message instead.
const RUNTIME_LIBRARY_NAME: &str = "libonnxruntime.so.1.28.0";

/// L2-normalization tolerance for the unit-norm check (task 2.1 acceptance criterion).
const NORM_EPS: f64 = 1e-5;

/// One `models.entries[]` record of onnx.yaml: name, vector_dim and declared files with sizes.
struct ModelEntry {
    /// models.default == this entry's name (bge-small-en-v1.5 is the registry + config default).
    is_default: bool,
    name: &'static str,
    vector_dim: usize,
    files: &'static [(&'static str, u64)],
}

/// All three models of the onnx.yaml registry, in registry order (Revision 2 of task 2.1).
const MODELS: &[ModelEntry] = &[
    ModelEntry {
        is_default: false,
        name: "bge-m3-int8",
        vector_dim: 1024,
        files: &[
            ("model.onnx", 724_923),
            ("model.onnx_data", 2_266_820_608),
            ("tokenizer.json", 17_082_821),
        ],
    },
    ModelEntry {
        is_default: true,
        name: "bge-small-en-v1.5",
        vector_dim: 384,
        files: &[
            ("model.onnx", 133_093_490),
            ("tokenizer.json", 711_396),
            ("tokenizer_config.json", 366),
        ],
    },
    ModelEntry {
        is_default: false,
        name: "paraphrase-multilingual-MiniLM-L12-v2",
        vector_dim: 384,
        files: &[("model.onnx", 470_301_610), ("tokenizer.json", 9_081_518)],
    },
];

/// The 50 fixed spike texts (hardcoded per task body): mostly short English queries, a block of
/// Russian sentences, medium paragraphs, one empty string (oracle edge case `Tokenize("")` ->
/// all-pad input) and two long texts that exceed MAX_LENGTH tokens to exercise the truncation
/// path. The same text set is used for every model; order matters only for JSONL line alignment
/// across runs.
const TEXTS: &[&str] = &[
    "What is retrieval augmented generation?",
    "How do vector databases store embeddings?",
    "Explain hybrid search with reciprocal rank fusion.",
    "Summarize the document about knowledge graphs.",
    "What does an MCP server expose to clients?",
    "Find chunks that mention SQLite full text search.",
    "How is the attention mask built for padded sequences?",
    "Compare brute-force and approximate nearest neighbor search.",
    "What is int8 quantization of neural networks?",
    "List the migrations applied to the knowledge base.",
    "Why is a disk-backed index needed on a laptop?",
    "Describe the ingestion pipeline from PDF to chunks.",
    "How does entity linking use CEL expressions?",
    "What is the difference between BM25 and cosine similarity?",
    "Explain WAL mode in SQLite.",
    "How are chunk offsets computed during parsing?",
    "What providers can generate embeddings locally?",
    "Describe the schema of the chunks table.",
    "How does the cache avoid recomputing vectors?",
    "What is a HNSW index and what do M and ef mean?",
    "Explain tokenization with a BPE model.",
    "How are timestamps normalized in documents?",
    "What happens when a document is re-synced?",
    "Describe the role of the ontology XML files.",
    "How does the server handle concurrent requests?",
    "What is graceful shutdown for an MCP transport?",
    "Explain the difference between precision and recall at k.",
    "How are external data files resolved by ONNX Runtime?",
    "What is mmap and why does it save RAM?",
    "Describe quantized HNSW with int8 vectors.",
    "What is a knowledge base and how does it differ from a data lake?",
    "Что такое ретривал-аугментированная генерация?",
    "Как работает полнотекстовый поиск в SQLite?",
    "Опишите конвейер загрузки документов и разбиения на чанки.",
    "Почему индексы векторов должны храниться на диске?",
    "Что такое квантование нейросетей до int8?",
    "Как вычисляется косинусное сходство двух векторов?",
    "Объясните, как работает токенизация BPE.",
    "Какие миграции были применены к базе знаний?",
    "Как устроен граф знаний и связывание сущностей?",
    "Опишите режим WAL в SQLite и его преимущества.",
    "Retrieval augmented generation combines a retriever over a corpus with a language model that conditions on the retrieved passages, so answers cite evidence instead of relying only on parametric memory. The retrieval step is typically hybrid: lexical matching plus dense vector search fused by reciprocal rank fusion, and the whole pipeline must stay fast enough for interactive use on a single laptop.",
    "A knowledge graph stores entities as nodes and typed relations as edges; entity resolution decides which surface forms point at the same node, and link prediction or expression-based rules can materialize missing edges. For a local assistant the graph is kept in SQLite next to the document store so that one process owns all state and no external service is required.",
    "Vector search over millions of embeddings on a 16 GB laptop cannot keep full-precision vectors resident: four gigabytes for one million 1024-dimensional floats already competes with the model, the database and the operating system. Disk-backed or memory-mapped indexes with quantized vectors therefore bound RAM by the index configuration instead of the corpus size, which is what makes approximate nearest neighbor search viable in this project.",
    "Chunking a document means splitting it into overlapping passages that are small enough to embed independently but large enough to stay self-contained. Offsets and sequence numbers are recorded per chunk so citations can point back at exact character ranges, and re-syncing a document deletes its old chunks before inserting the new ones inside one transaction.",
    "The tokenizer maps text to token ids without adding special tokens; sequences longer than the model limit are truncated from the tail and shorter ones are right-padded. The attention mask is then supplied with the input so that pooled representations do not mix in padding, although this spike deliberately replicates the oracle behaviour bit for bit including its quirks.",
    "ONNX Runtime loads a session graph once and runs it many times; weights stored in external data files next to the model are memory-mapped by default. The int8 quantized export of bge-m3 keeps accuracy close to float while roughly halving memory traffic, which matters because embedding throughput on laptop hardware is bound by bandwidth more than by arithmetic.",
    "",
    "The history of approximate nearest neighbor search goes back to locality sensitive hashing, but high-recall systems today almost always use hierarchical navigable small world graphs: a multi-layer proximity structure where the upper layers are sparse long-range shortcuts and the bottom layer holds all points. Construction parameters such as M control the number of edges per node while efConstruction controls how many candidates are kept in a priority queue during insertion, and query time is tuned with the efSearch beam width; recall at k against exact search is the standard yardstick because it measures what users actually see. Quantization enters both storage and distance computation: product quantization splits vectors into subspaces and codes them, while scalar int8 quantization applies a per-dimension or global scale, and the best configuration depends on the vector dimensionality and the memory budget of the machine. On a laptop with sixteen gigabytes the engineering trade is explicit — a full-precision one-million-vector index would need four gigabytes before any graph overhead, so systems either mmap the data, quantize it, or both. The benchmark protocol for this spike keeps the corpus synthetic but the metric honest: p50 and p95 latency over two hundred held-out queries plus recall at ten against a brute force ground truth computed in the same process.",
    "Гибридный поиск объединяет лексическое и векторное ранжирование, чтобы покрыть как точные совпадения терминов, так и смысловые близкие формулировки. Весовая сумма или взаимный ранговый слиянный подход позволяют не привязываться к одной метрике, а итоговый список цитирует источники, что важно для персональных ассистентов.",
];

fn main() {
    if let Err(e) = run(env::args().skip(1).collect()) {
        eprintln!("s2_onnx: FAIL — {e}");
        std::process::exit(1);
    }
}

/// Parsed command-line options (defaults relative to the repo root, where `cargo run` executes).
struct Opt {
    onnx_yaml: PathBuf,
    data_dir: PathBuf,
    /// Restrict the run to one registry model (its name in onnx.yaml); None = all three.
    model: Option<String>,
    /// Directory receiving `<model>.jsonl` per executed model (cross-run diff evidence).
    out: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<Opt, String> {
    let mut opt = Opt {
        onnx_yaml: PathBuf::from("../synopsis/configs/onnx.yaml"),
        data_dir: PathBuf::from("data"),
        model: None,
        out: None,
    };
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--onnx-yaml" => opt.onnx_yaml = next_value(&mut it, "--onnx-yaml")?.into(),
            "--data-dir" => opt.data_dir = next_value(&mut it, "--data-dir")?.into(),
            "--model" => opt.model = Some(next_value(&mut it, "--model")?),
            "--out" => opt.out = Some(next_value(&mut it, "--out")?.into()),
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    Ok(opt)
}

/// Next iterator element as the flag's value, or a usage error.
fn next_value<'a>(it: &mut impl Iterator<Item = &'a String>, flag: &str) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("missing value for {flag}"))
}

/// Per-model measurement record reported by run_model (the unit-norm deviation is printed inside
/// run_model and does not need to be kept).
struct ModelReport {
    /// Wall time of the first inference pass over all 50 texts (ms).
    total_ms: u128,
    /// First text only (includes session warm-up; ms).
    first_text_ms: u128,
    vectors: Vec<Vec<f32>>,
}

fn run(args: Vec<String>) -> Result<(), String> {
    let opt = parse_args(&args)?;

    // ── Step 1: registry transcription vs onnx.yaml (authoritative source).
    check_registry_transcription(&opt.onnx_yaml)?;

    // Which models to execute.
    let selected: Vec<&ModelEntry> = match &opt.model {
        Some(name) => {
            let m = MODELS.iter().find(|m| m.name == *name).ok_or_else(|| {
                let known = MODELS.iter().map(|m| m.name).collect::<Vec<_>>().join(", ");
                format!("unknown --model '{name}' (registry entries: {known})")
            })?;
            vec![m]
        }
        None => MODELS.iter().collect(),
    };

    // ── Step 2: artifacts present in data/ with the declared sizes (runtime + ALL models).
    let runtime_lib = opt.data_dir.join("onnxruntime").join(RUNTIME_LIBRARY_NAME);
    ensure_artifacts(&opt.onnx_yaml, &opt.data_dir, &runtime_lib)?;

    // ── Step 3: load the ONNX Runtime library from data/ (oracle mechanism, via ort), once.
    let lib_abs = absolute_path(&runtime_lib).map_err(|e| {
        format!(
            "resolve runtime library path {}: {e}",
            runtime_lib.display()
        )
    })?;
    let t0 = Instant::now();
    // Keep the Environment alive for the whole process: sessions created below borrow it.
    let _env_committed = ort::init_from(&lib_abs)
        .map_err(|e| format!("load ONNX Runtime from {}: {e}", lib_abs.display()))?
        .commit();
    println!(
        "PASS ort-init: loaded {} via load-dynamic (ort 2.0.0-rc.x, C API level 28 = ORT 1.28) in {} ms",
        lib_abs.display(),
        t0.elapsed().as_millis()
    );

    // ── Steps 4–5: per-model tokenizer + session + embeddings + checks.
    let mut reports: Vec<(&ModelEntry, ModelReport)> = Vec::with_capacity(selected.len());
    for model in &selected {
        let rep = run_model(model, &opt.data_dir)?;
        println!(
            "info [{}]: timing {} ms total for {} texts = {:.2} ms/text (incl. tokenization; batch size 1 like the oracle), first text incl. warm-up {} ms",
            model.name,
            rep.total_ms,
            TEXTS.len(),
            rep.total_ms as f64 / TEXTS.len() as f64,
            rep.first_text_ms
        );
        // Phase-end resident memory (informational in multi-model runs; the process VmPeak at the
        // end of a single-model run is that model's true peak — see module docs).
        println!(
            "info [{}]: RSS at phase end = {}",
            model.name,
            vm_field_display("VmRSS")
        );
        if let Some(out_dir) = &opt.out {
            fs::create_dir_all(out_dir)
                .map_err(|e| format!("create {}: {e}", out_dir.display()))?;
            let file = out_dir.join(format!("{}.jsonl", model.name));
            write_jsonl(&file, &rep.vectors)?;
            println!(
                "info [{}]: wrote {} JSONL lines to {}",
                model.name,
                TEXTS.len(),
                file.display()
            );
        }
        reports.push((model, rep));
    }

    // ── Reporting: peak RSS (process-wide VmPeak) + verdict.
    let single = selected.len() == 1;
    let label = if single {
        format!(
            "peak RSS of [{}] process = {}",
            selected[0].name,
            vm_field_display("VmPeak")
        )
    } else {
        format!(
            "process VmPeak (cumulative over {} models) = {}",
            selected.len(),
            vm_field_display("VmPeak")
        )
    };
    println!("info: {label}");

    for (model, _) in &reports {
        let tag = if model.is_default {
            " [registry default]"
        } else {
            ""
        };
        println!(
            "s2_onnx: OK [{}{tag}] — N=50 × dim={} embeddings finite, unit-norm ≤ eps, deterministic",
            model.name, model.vector_dim
        );
    }
    Ok(())
}

/// Run one registry model end-to-end: tokenizer + session + N=50 embeddings + acceptance checks.
fn run_model(model: &ModelEntry, data_dir: &Path) -> Result<ModelReport, String> {
    let model_dir = data_dir.join("models").join(model.name);

    // Tokenizer (oracle: <model dir>/tokenizer.json).
    let tokenizer_path = model_dir.join("tokenizer.json");
    let t0 = Instant::now();
    let tokenizer = Tokenizer::from_file(&tokenizer_path)
        .map_err(|e| format!("load tokenizer {}: {e}", tokenizer_path.display()))?;
    println!(
        "info [{}]: HF tokenizer.json loaded in {} ms",
        model.name,
        t0.elapsed().as_millis()
    );

    // Session (scope: dropped at the end of this function so multi-model runs do not accumulate).
    let model_path = model_dir.join("model.onnx");
    let t0 = Instant::now();
    let mut session = Session::builder()
        .map_err(|e| format!("session builder: {e}"))?
        .commit_from_file(&model_path)
        .map_err(|e| format!("create ONNX session from {}: {e}", model_path.display()))?;
    println!(
        "info [{}]: session created in {} ms",
        model.name,
        t0.elapsed().as_millis()
    );

    // Detect the graph I/O and select tensors exactly like the oracle provider does.
    let input_names = detect_input_names(&session)?;
    let output_name = select_output_name(&session)?;
    println!(
        "PASS session [{}]: inputs=[{}], declared outputs=[{}]; embedding output '{}'",
        model.name,
        input_names.join(", "),
        declared_outputs(&session).join(", "),
        output_name
    );

    // Embed the 50 fixed texts (oracle pipeline, batch size 1 per text).
    let t0 = Instant::now();
    // TEXTS is non-empty, so the i == 0 branch below always assigns before use.
    let mut first_text_ms: u128 = 0;
    let mut vectors = Vec::with_capacity(TEXTS.len());
    for (i, text) in TEXTS.iter().enumerate() {
        let ti = Instant::now();
        let v = embed_one(
            &mut session,
            &tokenizer,
            model.vector_dim,
            &input_names,
            &output_name,
            text,
        )?;
        if i == 0 {
            first_text_ms = ti.elapsed().as_millis();
        }
        vectors.push(v);
    }
    let total_ms = t0.elapsed().as_millis();

    // Check: dimension (registry vector_dim) + finiteness.
    for (i, v) in vectors.iter().enumerate() {
        if v.len() != model.vector_dim {
            return Err(format!(
                "[{}] vector {} has dim {}, expected {}",
                model.name,
                i,
                v.len(),
                model.vector_dim
            ));
        }
        if !v.iter().all(|x| x.is_finite()) {
            return Err(format!(
                "[{}] vector {} contains non-finite values",
                model.name, i
            ));
        }
    }
    println!(
        "PASS vectors [{}]: {} vectors dim={}, all finite (first text incl. warm-up {} ms)",
        model.name,
        vectors.len(),
        model.vector_dim,
        first_text_ms
    );

    // Check: unit norm within eps (oracle L2-normalizes in f64, casts back to f32).
    let max_dev = vectors
        .iter()
        .map(|v| l2_norm(v) - 1.0)
        .fold(0.0_f64, |acc, d| acc.max(d.abs()));
    if max_dev > NORM_EPS {
        return Err(format!(
            "[{}] unit-norm check: max deviation from 1 is {max_dev}, exceeds eps {NORM_EPS}",
            model.name
        ));
    }
    println!(
        "PASS norm [{}]: unit-norm within eps (max |‖v‖−1| = {max_dev:.3e} ≤ {NORM_EPS})",
        model.name
    );

    // Check: in-process determinism — a second pass must be bit-identical (design D5).
    for (i, text) in TEXTS.iter().enumerate() {
        let again = embed_one(
            &mut session,
            &tokenizer,
            model.vector_dim,
            &input_names,
            &output_name,
            text,
        )?;
        if again != vectors[i] {
            return Err(format!(
                "[{}] determinism: vector {} differs between two inference passes",
                model.name, i
            ));
        }
    }
    println!(
        "PASS determinism(in-process) [{}]: second inference pass bit-identical for {}/{} texts (cross-run check: run twice with --model and --out, then diff)",
        model.name,
        TEXTS.len(),
        TEXTS.len()
    );

    Ok(ModelReport {
        total_ms,
        first_text_ms,
        vectors,
    })
}

/// Assert that the transcribed registry constants still appear verbatim in onnx.yaml (runtime +
/// every model entry: name, vector_dim and per-file name/size_bytes), so a stale transcription
/// (yaml edited upstream) fails at start-up instead of silently drifting.
fn check_registry_transcription(yaml_path: &Path) -> Result<(), String> {
    let text =
        fs::read_to_string(yaml_path).map_err(|e| format!("read registry {yaml_path:?}: {e}"))?;
    let mut expected_literals: Vec<String> = vec![
        "version: \"1.28.0\"".to_string(),
        format!("library_name: \"{RUNTIME_LIBRARY_NAME}\""),
    ];
    // models.default must still point at the entry marked is_default in MODELS.
    let default = MODELS
        .iter()
        .find(|m| m.is_default)
        .ok_or("no default model in MODELS")?;
    expected_literals.push(format!("default: \"{}\"", default.name));
    for m in MODELS {
        expected_literals.push(format!("- name: {}", m.name));
        expected_literals.push(format!("vector_dim: {}", m.vector_dim));
        for (file, size) in m.files {
            expected_literals.push(format!("- name: {file}"));
            expected_literals.push(format!("size_bytes: {size}"));
        }
    }
    for lit in &expected_literals {
        if !text.contains(lit.as_str()) {
            return Err(format!(
                "registry transcription stale: {yaml_path:?} no longer contains `{lit}` — re-transcribe constants"
            ));
        }
    }
    println!(
        "PASS registry: transcribed constants consistent with {} (runtime {}, default model {}, 3 entries)",
        yaml_path.display(),
        RUNTIME_VERSION,
        default.name
    );
    Ok(())
}

/// Verify the runtime library and ALL three models' artifacts exist under data/ with the declared
/// sizes; check the .so embeds its version string. Mirrors the oracle downloader's size
/// verification (size_bytes in onnx.yaml).
fn ensure_artifacts(yaml_path: &Path, data_dir: &Path, runtime_lib: &Path) -> Result<(), String> {
    let mut total = 0_u64;

    // Runtime library (no size_bytes declared for the extracted .so in onnx.yaml — verify presence
    // + embedded version string instead).
    let lib_size = file_size(runtime_lib)?;
    total += lib_size;
    if !embedded_version_present(runtime_lib, RUNTIME_VERSION) {
        return Err(format!(
            "runtime library {} does not embed the declared version string '{RUNTIME_VERSION}' — wrong artifact?",
            runtime_lib.display()
        ));
    }

    // Model files: existence + exact size_bytes from onnx.yaml, for every registry entry.
    let mut file_count = 0usize;
    for m in MODELS {
        for (name, declared) in m.files {
            let p = data_dir.join("models").join(m.name).join(name);
            let size = file_size(&p)?;
            if size != *declared {
                return Err(format!(
                    "size mismatch for {}: {} bytes, onnx.yaml ({yaml_path:?}) declares {declared} — re-download or fix data/ (see data/README.md)",
                    p.display(),
                    size
                ));
            }
            total += size;
            file_count += 1;
        }
    }

    println!(
        "PASS artifacts: runtime lib ({} B, embeds \"{}\") + {} model files across 3 models — sizes match onnx.yaml; total {} MB",
        lib_size,
        RUNTIME_VERSION,
        file_count,
        total / 1024 / 1024
    );
    Ok(())
}

fn file_size(path: &Path) -> Result<u64, String> {
    fs::metadata(path).map(|m| m.len()).map_err(|e| {
        format!(
            "artifact missing {}: {e} — preload per data/README.md",
            path.display()
        )
    })
}

/// Scan the (~24 MB) shared library for its embedded version string; ORT builds carry e.g. "1.28.0".
fn embedded_version_present(path: &Path, version: &str) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    let suffixed = format!("{version}\n"); // version line as printed by `strings`
    let needle = suffixed.as_bytes();
    bytes.windows(needle.len()).any(|w| w == needle)
        || bytes
            .windows(version.len())
            .any(|w| w == version.as_bytes())
}

/// Absolute form of a (possibly CWD-relative) path — ort::init_from resolves relative paths
/// against the executable location, not the working directory.
fn absolute_path(p: &Path) -> std::io::Result<PathBuf> {
    if p.is_absolute() {
        Ok(p.to_path_buf())
    } else {
        Ok(env::current_dir()?.join(p))
    }
}

/// The model's declared inputs that the oracle provider knows, in canonical order (onnx.yaml /
/// onnx_provider.go `knownModelInputs`): input_ids + attention_mask required, token_type_ids only
/// when present (absent for the bge-m3 export, present for the BGE-small and MiniLM exports).
fn detect_input_names(session: &Session) -> Result<Vec<String>, String> {
    let declared: Vec<&str> = session.inputs().iter().map(|o| o.name()).collect();
    let mut names = Vec::new();
    for known in ["input_ids", "attention_mask", "token_type_ids"] {
        if declared.contains(&known) {
            names.push(known.to_string());
        }
    }
    if !names.iter().any(|n| n == "input_ids") || !names.iter().any(|n| n == "attention_mask") {
        return Err(format!(
            "model lacks required inputs input_ids and attention_mask (declared: {declared:?})"
        ));
    }
    Ok(names)
}

/// Oracle `selectOutputName`: sentence_embedding when available, otherwise last_hidden_state,
/// otherwise the single declared output.
fn select_output_name(session: &Session) -> Result<String, String> {
    let declared = declared_outputs(session);
    for preferred in ["sentence_embedding", "last_hidden_state"] {
        if declared.iter().any(|n| n == preferred) {
            return Ok(preferred.to_string());
        }
    }
    if declared.len() == 1 {
        // Single-output exports (e.g. token_embeddings); `first()` is Some by construction of the
        // branch above, so an empty fallback can never actually be returned.
        return Ok(declared.first().cloned().unwrap_or_default());
    }
    Err(format!(
        "no supported embedding output found (declared: {declared:?})"
    ))
}

fn declared_outputs(session: &Session) -> Vec<String> {
    session
        .outputs()
        .iter()
        .map(|o| o.name().to_string())
        .collect()
}

/// Oracle `infer` for one text: tokenize → truncate/pad to MAX_LENGTH (pad id 0, mask all ones) →
/// run the session → take the first vector of dim `vector_dim` → L2-normalize in f64, cast back.
fn embed_one(
    session: &mut Session,
    tokenizer: &Tokenizer,
    vector_dim: usize,
    input_names: &[String],
    output_name: &str,
    text: &str,
) -> Result<Vec<f32>, String> {
    let ids = tokenize(tokenizer, text)?;

    // Bind only the inputs the model declares (exports differ per model — see detect_input_names).
    // Each declared input is built in its own straight-line block so every data buffer is moved
    // exactly once.
    let mut inputs: Vec<(String, ort::value::DynValue)> = Vec::with_capacity(input_names.len());
    if input_names.iter().any(|n| n.as_str() == "input_ids") {
        let data: Box<[i64]> = ids.into_boxed_slice();
        push_input(&mut inputs, "input_ids", data)?;
    }
    if input_names.iter().any(|n| n.as_str() == "attention_mask") {
        // Oracle quirk replicated verbatim: mask is all ones over the full padded length.
        let data = vec![1_i64; MAX_LENGTH].into_boxed_slice();
        push_input(&mut inputs, "attention_mask", data)?;
    }
    if input_names.iter().any(|n| n.as_str() == "token_type_ids") {
        // Single sequence, zeros.
        let data = vec![0_i64; MAX_LENGTH].into_boxed_slice();
        push_input(&mut inputs, "token_type_ids", data)?;
    }

    let outputs = session
        .run(inputs)
        .map_err(|e| format!("ONNX run for text {:?}: {e}", &text[..text.len().min(40)]))?;

    let out_value = outputs
        .get(output_name)
        .ok_or_else(|| format!("session output '{output_name}' missing"))?;
    let (shape, data) = out_value
        .try_extract_tensor::<f32>()
        .map_err(|e| format!("extract {output_name} tensor: {e}"))?;

    // Oracle takes the [0, 0, :] vector — first row of dim `vector_dim` ([1, L, H] hidden states or
    // [1, H] pooled sentence_embedding).
    if data.len() < vector_dim || shape[0] != 1 {
        return Err(format!(
            "output tensor too small: expected at least {vector_dim} values with batch 1, got shape {shape:?}"
        ));
    }
    let mut result = data[..vector_dim].to_vec();

    // L2 normalize exactly like the oracle: f32 square products accumulated in f64, single cast.
    let mut acc: f64 = 0.0;
    for &v in &result {
        acc += (v * v) as f64;
    }
    let norm = acc.sqrt();
    if norm > 1e-9 {
        let scale = norm as f32;
        for x in &mut result {
            *x /= scale;
        }
    }
    Ok(result)
}

/// Build a [1, MAX_LENGTH] i64 tensor and append it to the session inputs under `name`.
fn push_input(
    inputs: &mut Vec<(String, ort::value::DynValue)>,
    name: &str,
    data: Box<[i64]>,
) -> Result<(), String> {
    let t = Tensor::<i64>::from_array(([1usize, MAX_LENGTH], data))
        .map_err(|e| format!("build {name} tensor: {e}"))?;
    inputs.push((name.to_string(), t.into_dyn()));
    Ok(())
}

/// Oracle `SugarmeTokenizer.Tokenize`: encode without special tokens, truncate to MAX_LENGTH,
/// right-pad with PAD_ID (0 on the tokenizer.json path).
fn tokenize(tokenizer: &Tokenizer, text: &str) -> Result<Vec<i64>, String> {
    let mut ids: Vec<i64> = if text.is_empty() {
        // Oracle Tokenize("") returns maxLength zeros without touching the encoder.
        vec![0_i64; MAX_LENGTH]
    } else {
        let enc = tokenizer
            .encode(text, false)
            .map_err(|e| format!("tokenize {:?}: {e}", &text[..text.len().min(40)]))?;
        enc.get_ids()
            .iter()
            .take(MAX_LENGTH)
            .map(|&id| id as i64)
            .collect()
    };
    ids.resize(MAX_LENGTH, PAD_ID);
    Ok(ids)
}

/// L2 norm in f64 (check-side computation; not the normalization path itself).
fn l2_norm(v: &[f32]) -> f64 {
    v.iter().map(|x| (*x * *x) as f64).sum::<f64>().sqrt()
}

/// One field of /proc/self/status (VmPeak/VmRSS, kB on Linux); "n/a" elsewhere.
fn vm_field_display(field: &str) -> String {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return format!("n/a ({field}, not Linux)");
    };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(&format!("{field}:")) {
            let kb: u64 = rest.trim().trim_end_matches(" kB").parse().unwrap_or(0);
            return format!("{:.1} MB ({} {} kB)", kb as f64 / 1024.0, field, kb);
        }
    }
    format!("n/a ({field})")
}

/// One JSON object per line: {"index": i, "vector": [f32...]} — deterministic shortest-round-trip
/// float formatting so two runs produce diffable files (cross-run bit-identity evidence).
fn write_jsonl(path: &Path, vectors: &[Vec<f32>]) -> Result<(), String> {
    let mut out = String::new();
    for (i, v) in vectors.iter().enumerate() {
        out.push_str(&format!("{{\"index\": {}, \"vector\": [", i));
        for (j, x) in v.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(&x.to_string());
        }
        out.push_str("]}\n");
    }
    fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))
}
