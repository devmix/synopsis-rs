import type {ReactNode} from 'react';

import {FEATURES} from '../../shared/lib/data';

import styles from './FeaturesSection.module.css';

export function FeaturesSection(): ReactNode {
  return (
    <section className={styles.features} id="features">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 02 · capabilities
        </p>
        <h2 className={styles.display} data-reveal>
          One job, done well.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Six systems working inside a single process — strict boundaries, no
          services to operate between them.
        </p>
        <div className={styles.featGrid}>
          {FEATURES.map((feature, i) => (
            <article
              key={feature.name}
              className={`${styles.featCard} ${['', styles.d1, styles.d2][i % 3] ?? ''}`}
              data-reveal>
              <div className={styles.featTop}>
                <span className={styles.featIcon}>{feature.icon}</span>
                <span className={styles.featNum}>{feature.num}</span>
              </div>
              <h3>{feature.name}</h3>
              <p>{feature.desc}</p>
              <div className={styles.chipRow}>
                {feature.tags.map((tag) => (
                  <span key={tag} className={styles.chip}>
                    {tag}
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
