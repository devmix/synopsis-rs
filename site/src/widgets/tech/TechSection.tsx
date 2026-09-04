import type {ReactNode} from 'react';

import {TECH_ROWS} from '../../shared/lib/data';

import styles from './TechSection.module.css';

export function TechSection(): ReactNode {
  return (
    <section className={styles.tech} id="tech">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 07 · tech stack
        </p>
        <h2 className={styles.display} data-reveal>
          Chosen for zero operations, not fashion.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Each component exists so the whole system stays one process and one
          file — and so it can still be rebuilt by a single engineer.
        </p>
        <div className={styles.tableWrap} data-reveal>
          <table className={styles.table}>
            <thead>
              <tr>
                <th scope="col">Component</th>
                <th scope="col">Role</th>
                <th scope="col">Why</th>
              </tr>
            </thead>
            <tbody>
              {TECH_ROWS.map((row) => (
                <tr key={row.name}>
                  <th scope="row">{row.name}</th>
                  <td>{row.role}</td>
                  <td>{row.why}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </section>
  );
}
