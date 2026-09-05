import {useEffect, useRef, useState} from 'react';
import type {ReactNode} from 'react';

import type {PipelineStage} from '../../shared/lib/types';

import styles from './PipelineSection.module.css';

const PIPELINE_STAGES: readonly PipelineStage[] = [
  {
    num: '01',
    title: 'Sources',
    desc: 'Markdown, JSON, and web pages from the configured sources. A startup reconcile plus a file watcher enqueue document diffs into document_jobs; a background worker processes the queue.',
    meta: '.md · .json · web',
    chips: ['Markdown', 'JSON', 'Web pages'],
  },
  {
    num: '02',
    title: 'Parse & chunk',
    desc: 'A format-specific parser turns every file into the same structured document; header-based or fixed-size chunking with overlap. Unchanged files (same SHA-256) are skipped entirely.',
    meta: 'dedup by sha-256',
    chips: ['Parser', 'Chunking', 'Dedup'],
  },
  {
    num: '03',
    title: 'Extract & link',
    desc: 'Local ONNX embeddings per chunk, ONNX NER, then entity and fact extraction; the same entity is linked across domains with CEL or an LLM — into a knowledge graph and a usearch hybrid index.',
    meta: 'onnx · background worker',
    chips: ['Embeddings', 'NER', 'Facts', 'usearch index'],
  },
  {
    num: '04',
    title: 'Knowledge graph + MCP',
    desc: 'Entities, relations, and approved facts are served as 12 read-only MCP tools over Streamable HTTP (/mcp), with the legacy HTTP+SSE pair also served.',
    meta: 'POST /mcp · GET /sse · POST /message',
    chips: ['12 tools', 'approved facts', 'Provenance'],
    final: true,
  },
];

export function PipelineSection(): ReactNode {
  const [openIdx, setOpenIdx] = useState(0);
  const bodyRefs = useRef<(HTMLDivElement | null)[]>([]);

  useEffect(() => {
    bodyRefs.current.forEach((el, i) => {
      if (el) el.style.maxHeight = i === openIdx ? `${el.scrollHeight}px` : '';
    });
  }, [openIdx]);

  useEffect(() => {
    const onResize = () => {
      bodyRefs.current.forEach((el, i) => {
        if (el && i === openIdx) el.style.maxHeight = `${el.scrollHeight}px`;
      });
    };
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, [openIdx]);

  return (
    <section className={styles.pipeline} id="pipeline">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 01 · pipeline
        </p>
        <h2 className={styles.display} data-reveal>
          Documents in → knowledge out.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Synopsis is one stage of a larger information pipeline — not a
          standalone assistant and not a search platform. Everything between
          the source files and 12 MCP tools runs in one Rust binary.
        </p>
        <div className={styles.capList}>
          {PIPELINE_STAGES.map((stage, i) => {
            const open = i === openIdx;
            return (
              <div
                key={stage.num}
                className={`${styles.cap} ${open ? styles.capOpen : ''} ${['', styles.d1, styles.d2, styles.d3][i] ?? ''}`}
                data-reveal>
                <button
                  type="button"
                  className={styles.capHead}
                  aria-expanded={open}
                  aria-controls={`pipeline-body-${stage.num}`}
                  onClick={() => setOpenIdx(open ? -1 : i)}>
                  <span className={styles.capNum}>/{stage.num}</span>
                  <span className={styles.capTitle}>{stage.title}</span>
                  <span className={styles.capTags}>{stage.meta}</span>
                  <span className={styles.capIcon} aria-hidden="true">
                    +
                  </span>
                </button>
                <div
                  id={`pipeline-body-${stage.num}`}
                  ref={(el) => {
                    bodyRefs.current[i] = el;
                  }}
                  className={styles.capBody}>
                  <div className={styles.capInner}>
                    <div>
                      <p>{stage.desc}</p>
                      <div className={styles.chipRow}>
                        {stage.chips.map((chip) => (
                          <span key={chip} className={styles.chip}>
                            {chip}
                          </span>
                        ))}
                      </div>
                    </div>
                  </div>
                </div>
              </div>
            );
          })}
        </div>
      </div>
    </section>
  );
}
