import type {ReactNode} from 'react';

import type {TechRow} from '../../shared/lib/types';

import styles from './TechSection.module.css';

const TECH_ROWS: readonly TechRow[] = [
  {name: 'Rust 1.96 (pinned)', role: 'One self-contained binary', why: 'The toolchain is pinned in rust-toolchain.toml; SQLite with FTS5 is compiled in-tree, so a build cannot silently lack FTS5. No C interop.'},
  {name: 'tokio + axum', role: 'Async runtime + HTTP', why: 'One process serves the MCP endpoints, the job-queue worker, and the file watcher — no services to operate between them.'},
  {name: 'rusqlite (bundled SQLite + FTS5)', role: 'Storage + lexical search', why: 'WAL mode keeps serving reads while ingestion writes; BM25 over search_text is the lexical leg of hybrid search.'},
  {name: 'ONNX Runtime (ort)', role: 'Local embeddings on CPU', why: 'No API keys — int8-quantized bge models run fully offline; the runtime and weights download on demand, verified by URL + size + SHA-256.'},
  {name: 'usearch', role: 'Disk-backed ANN', why: 'HNSW on mmap, scalar-quantized, WAL + segments — 1M × 1024-dim vectors fit a 16 GB laptop, and the query path never loads the embedding model.'},
  {name: 'rmcp', role: 'MCP protocol', why: 'Streamable HTTP (/mcp) with the legacy HTTP+SSE pair also served — declarative, self-documenting tools for Claude Desktop, Cursor, and any MCP client.'},
  {name: 'CEL · petgraph · minijinja', role: 'Linking · graph · prompts', why: 'CEL expressions drive deterministic entity linking, petgraph indexes the knowledge graph, minijinja renders the LLM prompt templates.'},
  {name: 'jiff · clap · tracing', role: 'UTC dates · CLI · logging', why: 'RFC 3339 timestamps, a flag-driven CLI surface, and tracing in the binary only — library crates stay logger-less.'},
];

export function TechSection(): ReactNode {
  return (
    <section className={styles.tech} id="tech">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 07 · tech stack
        </p>
        <h2 className={styles.display} data-reveal>
          Chosen for zero operations, not fashion.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Each component exists so the whole system stays one process on local
          files — and so it can still be rebuilt by a single engineer.
        </p>
        <div className={styles.tableWrap} data-reveal>
          <table className={styles.table}>
            <thead>
              <tr>
                <th scope="col">Component</th>
                <th scope="col">Role</th>
                <th scope="col">Why</th>
              </tr>
            </thead>
            <tbody>
              {TECH_ROWS.map((row) => (
                <tr key={row.name}>
                  <th scope="row">{row.name}</th>
                  <td>{row.role}</td>
                  <td>{row.why}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </section>
  );
}
