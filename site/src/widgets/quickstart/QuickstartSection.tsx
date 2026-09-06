import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import type {QuiStep, TermLine} from '../../shared/lib/types';
import {TONE_CLASS, TrafficDots} from '../../shared/ui/terminal';

import styles from './QuickstartSection.module.css';

const QUISTEPS: readonly QuiStep[] = [
  {
    cmd: 'synopsis onnx-runtime install',
    title: 'Install the runtime',
    desc: 'One-time setup: downloads the ONNX Runtime library per workspace/configs/onnx.yaml — URL, size, and SHA-256 verified — for fully local embeddings.',
  },
  {
    cmd: 'synopsis model download',
    title: 'Download the model',
    desc: 'Fetches the default embedding model from the onnx.yaml registry and verifies it before first use.',
  },
  {
    cmd: 'synopsis serve',
    title: 'Serve',
    desc: 'Starts the MCP server on port 8080 (preset "default") — startup reconcile plus a file watcher keep the knowledge base fresh.',
  },
];

const QUICKSTART_LINES: readonly TermLine[] = [
  {text: '$ synopsis onnx-runtime install', tone: 'cmd'},
  {text: '✓ onnxruntime — cpu · linux/amd64 · sha-256 ok', tone: 'ok'},
  {text: '$ synopsis model download', tone: 'cmd'},
  {text: '✓ model bge-small-en-v1.5 · 384 dims · sha-256 ok', tone: 'ok'},
  {text: '$ synopsis serve', tone: 'cmd'},
  {text: '✓ mcp server → http://localhost:8080/mcp · legacy /sse', tone: 'ok'},
  {text: '$ curl -s http://localhost:8080/health', tone: 'cmd'},
  {text: '{"status":"ok"}', tone: 'hl'},
];

export function QuickstartSection(): ReactNode {
  return (
    <section className={styles.quickstart} id="quickstart">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 04 · quick start
        </p>
        <h2 className={styles.display} data-reveal>
          From binary to MCP tools.
        </h2>
        <div className={styles.qsGrid}>
          <div>
            {QUISTEPS.map((step, i) => (
              <div
                key={step.cmd}
                className={`${styles.qsStep} ${['', styles.d1, styles.d2][i] ?? ''}`}
                data-reveal>
                <span className={styles.qsNum} aria-hidden="true">
                  {String(i + 1).padStart(2, '0')}
                </span>
                <div>
                  <h3>
                    {step.title}{' '}
                    <code className={styles.qsCmd}>{step.cmd}</code>
                  </h3>
                  <p>{step.desc}</p>
                </div>
              </div>
            ))}
            <p data-reveal className={styles.d3}>
              <Link className={styles.toolLink} to="/docs/quickstart">
                Read the full quickstart →
              </Link>
            </p>
          </div>
          <div className={styles.d2} data-reveal>
            <div className={styles.qsCode}>
              <div className={styles.qsCodeHead}>
                <TrafficDots />
                <span className={styles.qsCodeTitle}>synopsis — zsh</span>
              </div>
              <pre className={styles.qsCodePre}>
                {QUICKSTART_LINES.map((line) => (
                  <span
                    key={line.text}
                    className={`${styles.codeLine} ${TONE_CLASS[line.tone]}`}>
                    {line.text}
                  </span>
                ))}
              </pre>
            </div>
            <div className={styles.qsJson}>
              <p className={styles.qsJsonLabel}>
                <b>→</b> first mcp call — POST /mcp · tools/call
              </p>
              <pre className={styles.qsCodePre}>
                {'{\n  "jsonrpc": "2.0",\n  "id": 1,\n  "method": "tools/call",\n  "params": {\n    "name": '}
                <span className={TONE_CLASS.hl}>{"search"}</span>
                {',\n    "arguments": {\n      "query": "vacation policy",\n      "domain": "hr"\n    }\n  }\n}'}
              </pre>
            </div>
          </div>
        </div>
      </div>
    </section>
  );
}
