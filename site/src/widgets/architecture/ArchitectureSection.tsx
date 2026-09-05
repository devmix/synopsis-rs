import type {ReactNode} from 'react';

import useBaseUrl from '@docusaurus/useBaseUrl';

import styles from './ArchitectureSection.module.css';

export function ArchitectureSection(): ReactNode {
  const svgSrc = useBaseUrl('img/architecture-landing.svg');

  return (
    <section className={styles.architecture} id="architecture">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 00 · architecture
        </p>
        <h2 className={styles.display} data-reveal>
          Eleven crates. One binary. Zero services.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Eleven workspace crates — config, db, vectors, embedding, ingestion,
          graph, search, mcp, llm, utils, cli — are wired into a fixed
          dependency graph and compiled into one Rust binary. Documents are
          parsed, chunked, embedded, and run through NER into a knowledge graph
          with a hybrid FTS5 + usearch index. State lives in a per-dataset
          knowledge.db, a global cache.db, and usearch index files — served to
          AI agents over MCP.
        </p>
        <div className={styles.svgWrap} data-reveal>
          <img
            src={svgSrc}
            alt="Architecture of synopsis[memex]: eleven Rust crates from sources through ingestion to MCP tools for AI agents"
            loading="lazy"
          />
        </div>
      </div>
    </section>
  );
}
