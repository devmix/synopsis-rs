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
          All-in-One. Zero external services.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Upstream sources are cleaned and normalized into domain-tagged documents.
          Inside one Go binary, the sync pipeline parses, chunks, embeds and runs NER
          to build entities and a knowledge graph with a hybrid FTS5 + vector index.
          Everything persists in a single SQLite file, served to AI agents over MCP.
        </p>
        <div className={styles.svgWrap} data-reveal>
          <img
            src={svgSrc}
            alt="Architecture of synopsis[memex]: from raw sources through the sync pipeline to MCP tools for AI agents"
            loading="lazy"
          />
        </div>
      </div>
    </section>
  );
}
