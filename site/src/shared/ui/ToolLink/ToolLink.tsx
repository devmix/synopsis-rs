import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import styles from './ToolLink.module.css';

export interface ToolLinkProps {
  to?: string;
  href?: string;
  children: ReactNode;
  className?: string;
}

export function ToolLink({to, href, children, className}: ToolLinkProps) {
  const classes = [styles.toolLink];
  if (className !== undefined) {
    classes.push(className);
  }
  return (
    <Link to={to} href={href} className={classes.join(' ')}>
      {children}
    </Link>
  );
}
