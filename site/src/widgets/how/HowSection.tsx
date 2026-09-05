import type {ReactNode} from 'react';

import type {FlowStep} from '../../shared/lib/types';

import styles from './HowSection.module.css';

const RUN_STEPS: readonly FlowStep[] = [
  {
    title: 'Obtain the binary',
    desc:
      'Prebuilt Gitea Release archive for the 5 targets (Linux amd64/arm64, ' +
      'Windows amd64, macOS arm64) — or cargo build --release from the pinned ' +
      'Rust 1.96.0 toolchain.',
  },
  {
    title: 'Install the runtime',
    desc: 'synopsis onnx-runtime install downloads the ONNX Runtime library per onnx.yaml — URL, size, and SHA-256 verified.',
  },
  {
    title: 'Download the model',
    desc: 'synopsis model download fetches the default embedding model into workspace/models/.',
  },
  {
    title: 'Serve',
    desc: 'synopsis serve starts the MCP server on :8080 — preset default, config auto-searched, initial ingest plus file watching.',
  },
];

const CLIENT_STEPS: readonly FlowStep[] = [
  {
    title: 'Streamable HTTP',
    desc: 'Point any MCP client at /mcp — the primary transport: JSON-RPC over POST, responses as plain JSON or an SSE stream.',
  },
  {
    title: 'Legacy HTTP+SSE',
    desc: 'GET /sse opens the stream, POST /message?sessionId=… carries the calls — the legacy wire contract, still served.',
  },
  {
    title: 'Verify',
    desc: 'GET /health returns status, version, and knowledge-base counters.',
  },
];

export function HowSection(): ReactNode {
  const panel = (
    badge: string,
    title: string,
    steps: readonly FlowStep[],
    delay: string,
  ): ReactNode => (
    <div className={`${styles.howPanel} ${delay}`} data-reveal>
      <div className={styles.howPanelHead}>
        <span className={styles.howBadge}>{badge}</span>
        <h3>{title}</h3>
      </div>
      <div className={styles.howSteps}>
        {steps.map((step) => (
          <div key={step.title} className={styles.howStep}>
            <h4>{step.title}</h4>
            <p>{step.desc}</p>
          </div>
        ))}
      </div>
    </div>
  );
  return (
    <section className={styles.how} id="how">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 03 · how it works
        </p>
        <h2 className={styles.display} data-reveal>
          From binary to MCP client.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          One binary, five steps — obtain it, install the runtime, download the
          model, serve, then connect a client. Everything runs locally.
        </p>
        <div className={styles.howGrid}>
          {panel('1', 'Obtain & run', RUN_STEPS, '')}
          {panel('2', 'Connect a client', CLIENT_STEPS, styles.d2)}
        </div>
      </div>
    </section>
  );
}
