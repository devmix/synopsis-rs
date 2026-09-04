import type {ReactNode} from 'react';

import {USE_CASES} from '../../shared/lib/data';

import styles from './UseCasesSection.module.css';

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
