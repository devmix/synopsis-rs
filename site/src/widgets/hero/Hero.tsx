import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import {GITHUB_RELEASES_URL, GITHUB_URL, TERMINAL_LINES} from '../../shared/lib/data';
import {useScrambleTitle} from '../../shared/lib/hooks/useScrambleTitle';
import {useTerminalTypewriter} from '../../shared/lib/hooks/useTerminalTypewriter';
import {TrafficDots} from '../../shared/ui/terminal/TrafficDots';

import styles from './Hero.module.css';

export function Hero(): ReactNode {
  const termBodyRef = useTerminalTypewriter(TERMINAL_LINES);
  const titleRef = useScrambleTitle();
  return (
    <header className={styles.hero}>
      <div className={styles.wrap}>
        <div className={styles.heroGrid}>
          <div>
            <p className={styles.eyebrow}>
              <b>[ open source ]</b> single binary · onnx · llm · sqlite · mcp
            </p>
            <h1
              className={styles.heroTitle}
              ref={titleRef}
              aria-label="synopsis [memex]">
              <span data-text="synopsis">synopsis</span>
              <br />
              <span className={styles.hl} data-text="[memex]">
                [memex]
              </span>
            </h1>
            <p className={styles.heroSub}>
              <strong>Structured information for AI agents via MCP.</strong>
              One Go binary — hybrid search, an in-memory knowledge graph, and
              ontology-driven linking in a single process, served to AI agents
              over MCP. No Python, no Postgres, no Docker.
            </p>
            <div className={styles.heroActions}>
              <Link className={`${styles.btn} ${styles.btnPrimary}`} to="/docs/intro">
                GET STARTED →
              </Link>
              <a
                className={`${styles.btn} ${styles.btnGhost}`}
                href={GITHUB_URL}
                target="_blank"
                rel="noreferrer">
                GITHUB ↗
              </a>
              <a
                className={`${styles.btn} ${styles.btnGhost}`}
                href={GITHUB_RELEASES_URL}
                target="_blank"
                rel="noreferrer">
                RELEASES ↗
              </a>
            </div>
            <div className={styles.heroMeta}>
              <span>
                Engine — <b>Go 1.25</b>
              </span>
              <span>
                Protocol — <b>MCP / SSE</b>
              </span>
              <span>
                Store — <b>SQLite</b>
              </span>
              <span>
                Embeddings — <b>ONNX / Prose / LLM</b>
              </span>
              <span>
                Linking — <b>CEL / Equals / LLM</b>
              </span>
              <span>
                Ontology — <b>Domains / Relations</b>
              </span>
              <span>
                LLM — <b>OpenAI API [local/remote]</b>
              </span>
            </div>
          </div>

          <div className={styles.termCol}>
            <div className={styles.statusPill}>
              <i aria-hidden="true" />
              MCP ONLINE
            </div>
            <div className={styles.terminal} role="img" aria-label="Terminal showing synopsis built, synced, and served as an MCP server">
              <div className={styles.termHead}>
                <TrafficDots />
                <span className={styles.termTitle}>synopsis — zsh · :8080/sse</span>
              </div>
              <div className={styles.termBody} ref={termBodyRef} />
            </div>
          </div>
        </div>
      </div>
    </header>
  );
}
