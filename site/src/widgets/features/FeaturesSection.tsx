import type {ReactNode} from 'react';

import type {Feature} from '../../shared/lib/types';
import {BinaryIcon, GraphIcon, LinkIcon, McpIcon, SearchIcon} from '../../shared/ui/icons';

import styles from './FeaturesSection.module.css';

const FEATURES: readonly Feature[] = [
  {
    num: '/01',
    icon: <SearchIcon />,
    name: 'Hybrid Search',
    desc: 'Lexical (FTS5 BM25) and semantic (usearch HNSW) retrieval run in parallel, fused with Reciprocal Rank Fusion and reranked by recency and authority.',
    tags: ['FTS5/BM25', 'usearch', 'RRF'],
  },
  {
    num: '/02',
    icon: <GraphIcon />,
    name: 'Knowledge Graph',
    desc: 'Entities from ONNX NER, cross-domain links from an LLM, CEL linkers for the deterministic cases — a petgraph index serves dossiers, relations, and links.',
    tags: ['ONNX NER', 'CEL linkers', 'petgraph'],
  },
  {
    num: '/03',
    icon: <McpIcon />,
    name: 'MCP Server',
    desc: 'Twelve read-only tools over Streamable HTTP (/mcp) with the legacy HTTP+SSE pair (GET /sse + POST /message) also served — built for agents, not browsers.',
    tags: ['Streamable HTTP', 'legacy HTTP+SSE', '12 tools'],
  },
  {
    num: '/04',
    icon: <BinaryIcon />,
    name: 'Disk-Backed ANN',
    desc: 'usearch HNSW on mmap, scalar-quantized, with WAL + segments and background compaction — the query path never loads the embedding model.',
    tags: ['usearch HNSW', 'mmap', 'WAL + segments'],
  },
  {
    num: '/05',
    icon: <BinaryIcon />,
    name: 'Self-Contained Build',
    desc: 'SQLite with FTS5 is compiled in-tree — no C interop, no system dependencies. The ONNX runtime and model weights download on demand, verified by URL + size + SHA-256.',
    tags: ['bundled SQLite', 'no C interop', 'sha-256 verified'],
  },
  {
    num: '/06',
    icon: <LinkIcon />,
    name: 'Document Job Queue',
    desc: 'A startup reconcile and a file watcher enqueue document diffs into document_jobs; a background worker processes the queue with retries and status reporting.',
    tags: ['document_jobs', 'background worker', 'retries'],
  },
];

export function FeaturesSection(): ReactNode {
  return (
    <section className={styles.features} id="features">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 02 · capabilities
        </p>
        <h2 className={styles.display} data-reveal>
          One job, done well.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Six systems inside one Rust binary — strict boundaries, no services
          to operate between them.
        </p>
        <div className={styles.featGrid}>
          {FEATURES.map((feature, i) => (
            <article
              key={feature.name}
              className={`${styles.featCard} ${['', styles.d1, styles.d2][i % 3] ?? ''}`}
              data-reveal>
              <div className={styles.featTop}>
                <span className={styles.featIcon}>{feature.icon}</span>
                <span className={styles.featNum}>{feature.num}</span>
              </div>
              <h3>{feature.name}</h3>
              <p>{feature.desc}</p>
              <div className={styles.chipRow}>
                {feature.tags.map((tag) => (
                  <span key={tag} className={styles.chip}>
                    {tag}
                  </span>
                ))}
              </div>
            </article>
          ))}
        </div>
      </div>
    </section>
  );
}
