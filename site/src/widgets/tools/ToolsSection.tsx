import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import {MCP_TOOLS} from '../../shared/lib/data';

import styles from './ToolsSection.module.css';

export function ToolsSection(): ReactNode {
  return (
    <section className={styles.tools} id="tools">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 05 · mcp tools
        </p>
        <h2 className={styles.display} data-reveal>
          Twelve tools. One protocol.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Everything an agent needs to read the knowledge base — declarative
          JSON schemas, cursor pagination, and full provenance on every
          answer.
        </p>
        <div className={styles.toolsGrid}>
          {MCP_TOOLS.map((tool, i) => (
            <Link
              key={tool.name}
              to={tool.href}
              className={`${styles.toolCard} ${['', styles.d1, styles.d2][i % 3] ?? ''}`}
              data-reveal>
              <span className={styles.toolTop}>
                <span className={styles.toolIdx}>{String(i + 1).padStart(2, '0')}</span>
                <span className={styles.toolCat}>{tool.cat}</span>
              </span>
              <span className={styles.toolName}>{tool.name}</span>
              <span className={styles.toolDesc}>{tool.desc}</span>
              <span className={styles.toolLink}>docs →</span>
            </Link>
          ))}
        </div>
      </div>
    </section>
  );
}
