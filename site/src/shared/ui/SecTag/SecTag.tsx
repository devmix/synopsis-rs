import type {ReactNode} from 'react';

import styles from './SecTag.module.css';

export interface SecTagProps {
  children: ReactNode;
}

export function SecTag({children}: SecTagProps) {
  return (
    <p className={styles.secTag}>
      <b>//</b> {children}
    </p>
  );
}
