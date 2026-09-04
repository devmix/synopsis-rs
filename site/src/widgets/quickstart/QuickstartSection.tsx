import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import {QUICKSTART_LINES, QUISTEPS} from '../../shared/lib/data';
import {TONE_CLASS, TrafficDots} from '../../shared/ui/terminal';

import styles from './QuickstartSection.module.css';

export function QuickstartSection(): ReactNode {
  return (
    <section className={styles.quickstart} id="quickstart">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 04 · quick start
        </p>
        <h2 className={styles.display} data-reveal>
          Clone to MCP tools in five minutes.
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
                <span className={styles.qsCodeTitle}>synopsis — make</span>
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
                <b>→</b> first mcp call — POST /message · tools/call
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
