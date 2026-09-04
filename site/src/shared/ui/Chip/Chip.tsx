import type {ReactNode} from 'react';

import styles from './Chip.module.css';

export interface ChipProps {
  children: ReactNode;
  className?: string;
}

export function Chip({children, className}: ChipProps) {
  return (
    <span className={className ? `${styles.chip} ${className}` : styles.chip}>
      {children}
    </span>
  );
}

export interface ChipRowProps {
  children: ReactNode;
  className?: string;
}

export function ChipRow({children, className}: ChipRowProps) {
  return (
    <div className={className ? `${styles.chipRow} ${className}` : styles.chipRow}>
      {children}
    </div>
  );
}
