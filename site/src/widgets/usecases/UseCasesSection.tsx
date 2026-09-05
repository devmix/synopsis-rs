import type {ReactNode} from 'react';

import type {UseCase} from '../../shared/lib/types';

import styles from './UseCasesSection.module.css';

const USE_CASES: readonly UseCase[] = [
  {
    tag: 'HR',
    title: 'People operations',
    desc: 'Vacation, hiring, benefits — ask “how does the policy apply to contractors?” and get the answer with the exact source quote.',
    chips: ['policy', 'role', 'process'],
  },
  {
    tag: 'IT',
    title: 'Infrastructure ops',
    desc: 'Runbooks, incidents, change records — find the last fix for a service outage across the wiki and ticket history.',
    chips: ['service', 'incident', 'runbook'],
  },
  {
    tag: 'Product',
    title: 'Product teams',
    desc: 'Specs, roadmaps, feedback — keep features, decisions, and owners consistent in one queryable place.',
    chips: ['feature', 'decision', 'release'],
  },
  {
    tag: 'Engineering',
    title: 'Engineering',
    desc: 'Design docs, ADRs, post-mortems — the same system or person linked across code, prose, and tickets.',
    chips: ['adr', 'system', 'post-mortem'],
  },
];

export function UseCasesSection(): ReactNode {
  return (
    <section className={styles.usecases} id="usecases">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 06 · use cases
        </p>
        <h2 className={styles.display} data-reveal>
          Built for how teams actually store knowledge.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Domain tags decide what gets extracted and how it links — the same
          engine serves every department.
        </p>
        <div className={styles.ucGrid}>
          {USE_CASES.map((useCase, i) => (
            <article
              key={useCase.tag}
              className={`${styles.ucCard} ${['', styles.d1, styles.d2, styles.d3][i] ?? ''}`}
              data-reveal>
              <span className={styles.ucTag}>{useCase.tag}</span>
              <h3>{useCase.title}</h3>
              <p>{useCase.desc}</p>
              <div className={styles.chipRow}>
                {useCase.chips.map((chip) => (
                  <span key={chip} className={styles.chip}>
                    {chip}
                  </span>
                ))}
              </div>
            </article>
          ))}
        </div>
      </div>
    </section>
  );
}
