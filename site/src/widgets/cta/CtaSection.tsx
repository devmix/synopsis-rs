import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import {GITHUB_URL} from '../../shared/lib/data';

import styles from './CtaSection.module.css';

export function CtaSection(): ReactNode {
  return (
    <section className={styles.cta} id="cta">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 08 · get started
        </p>
        <h2 className={styles.ctaTitle} data-reveal>
          Give your agents real context.
        </h2>
        <p className={styles.ctaCopy} data-reveal>
          Point any MCP client at localhost:8080/sse. Clean documents in, a
          queryable knowledge base out — about five minutes of your time.
        </p>
        <div className={styles.ctaActions} data-reveal>
          <Link className={`${styles.btn} ${styles.btnPrimary}`} to="/docs/quickstart">
            GET STARTED →
          </Link>
          <a
            className={`${styles.btn} ${styles.btnGhost}`}
            href={GITHUB_URL}
            target="_blank"
            rel="noreferrer">
            GITHUB ↗
          </a>
        </div>
        <p className={styles.ctaPrompt} data-reveal>
          <b>$</b> synopsis serve — mcp http://localhost:8080/sse
        </p>
      </div>
    </section>
  );
}
