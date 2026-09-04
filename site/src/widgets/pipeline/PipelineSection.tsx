import {useEffect, useRef, useState} from 'react';
import type {ReactNode} from 'react';

import {PIPELINE_STAGES} from '../../shared/lib/data';

import styles from './PipelineSection.module.css';

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
          Clean data in → MCP tools out.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Synopsis is one stage of a larger information pipeline — not a
          standalone assistant and not a search platform. Everything between
          normalized documents and 12 MCP tools belongs here.
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
