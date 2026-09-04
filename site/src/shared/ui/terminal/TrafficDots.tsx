import type {ReactNode} from 'react';

import styles from './terminal.module.css';

export function TrafficDots(): ReactNode {
  return (
    <>
      <span className={styles.tdotR} aria-hidden="true" />
      <span className={styles.tdotY} aria-hidden="true" />
      <span className={styles.tdotG} aria-hidden="true" />
    </>
  );
}
