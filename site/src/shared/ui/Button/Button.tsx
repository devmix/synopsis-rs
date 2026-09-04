import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import styles from './Button.module.css';

export interface ButtonProps {
  variant: 'primary' | 'ghost';
  href: string;
  children: ReactNode;
  className?: string;
}

export function Button({variant, href, children, className}: ButtonProps) {
  const classes = [
    styles.btn,
    variant === 'primary' ? styles.btnPrimary : styles.btnGhost,
  ];
  if (className !== undefined) {
    classes.push(className);
  }
  return (
    <Link href={href} className={classes.join(' ')}>
      {children}
    </Link>
  );
}
