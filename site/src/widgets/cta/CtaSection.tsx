import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import styles from './CtaSection.module.css';

const GITHUB_URL = 'https://github.com/devmix/synopsis-rs';

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
          Point any MCP client at localhost:8080/mcp (or /sse for legacy
          clients). Documents in, a queryable knowledge base out — about five
          minutes of your time.
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
          <b>$</b> synopsis serve — mcp http://localhost:8080/mcp · legacy /sse
        </p>
      </div>
    </section>
  );
}
