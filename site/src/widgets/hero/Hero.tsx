import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import type {TermLine} from '../../shared/lib/types';
import {useScrambleTitle} from '../../shared/lib/hooks/useScrambleTitle';
import {useTerminalTypewriter} from '../../shared/lib/hooks/useTerminalTypewriter';
import {TrafficDots} from '../../shared/ui/terminal/TrafficDots';

import styles from './Hero.module.css';

const GITHUB_URL = 'https://github.com/devmix/synopsis-rs';
const GITHUB_RELEASES_URL = `${GITHUB_URL}/releases`;

const TERMINAL_LINES: readonly TermLine[] = [
  {text: '$ ./synopsis onnx-runtime install', tone: 'cmd'},
  {text: '✓ onnxruntime — cpu · linux/amd64', tone: 'ok'},
  {text: '$ ./synopsis model download', tone: 'cmd'},
  {text: '✓ model bge-small-en-v1.5 · 384 dims · sha-256 ok', tone: 'ok'},
  {text: '$ ./synopsis serve', tone: 'cmd'},
  {text: '✓ mcp server → :8080/mcp · legacy /sse', tone: 'ok'},
  {text: '✓ 1,284 docs · 12,902 chunks embedded (onnx)', tone: 'ok'},
  {text: '✓ 3,411 entities · 9,027 facts · graph loaded', tone: 'ok'},
  {text: '$ search {"query":"vacation policy"}', tone: 'cmd'},
  {text: '✓ 10 chunks · fts5 + usearch · rrf', tone: 'hl'},
];

export function Hero(): ReactNode {
  const termBodyRef = useTerminalTypewriter(TERMINAL_LINES);
  const titleRef = useScrambleTitle();
  return (
    <header className={styles.hero}>
      <div className={styles.wrap}>
        <div className={styles.heroGrid}>
          <div>
            <p className={styles.eyebrow}>
              <b>[ open source ]</b> single binary · rust · onnx · sqlite · mcp
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
              One Rust binary — hybrid search and a knowledge graph in a
              single process, built for a 16 GB laptop. No external services.
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
                Engine — <b>Rust 1.96 (pinned)</b>
              </span>
              <span>
                Protocol — <b>MCP · HTTP + SSE</b>
              </span>
              <span>
                Store — <b>SQLite · FTS5</b>
              </span>
              <span>
                Vectors — <b>usearch HNSW</b>
              </span>
              <span>
                Embeddings — <b>ONNX · local</b>
              </span>
              <span>
                Linking — <b>CEL / Equals / LLM</b>
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
            <div className={styles.terminal} role="img" aria-label="Terminal showing synopsis installing the runtime, downloading the model, and serving as an MCP server">
              <div className={styles.termHead}>
                <TrafficDots />
                <span className={styles.termTitle}>synopsis — zsh · :8080/mcp</span>
              </div>
              <div className={styles.termBody} ref={termBodyRef} />
            </div>
          </div>
        </div>
      </div>
    </header>
  );
}
