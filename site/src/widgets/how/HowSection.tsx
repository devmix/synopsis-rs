import type {ReactNode} from 'react';

import {INGESTION_STEPS, SEARCH_STEPS} from '../../shared/lib/data';
import type {FlowStep} from '../../shared/lib/types';

import styles from './HowSection.module.css';

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
          Ingestion → graph → search.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Two paths, one process. The sync pass builds the knowledge base; the
          query pass answers agents with ranked, enriched, provenance-carrying
          results.
        </p>
        <div className={styles.howGrid}>
          {panel('1', 'Ingestion pipeline', INGESTION_STEPS, '')}
          {panel('2', 'Query execution', SEARCH_STEPS, styles.d2)}
        </div>
      </div>
    </section>
  );
}
