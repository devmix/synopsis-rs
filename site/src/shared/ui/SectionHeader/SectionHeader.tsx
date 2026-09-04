import type {ReactNode} from 'react';

import {SecTag} from '../SecTag';

import styles from './SectionHeader.module.css';

export interface SectionHeaderProps {
  tag: string;
  title: ReactNode;
  lead?: ReactNode;
}

export function SectionHeader({tag, title, lead}: SectionHeaderProps) {
  return (
    <>
      <SecTag>{tag}</SecTag>
      <h2 className={styles.display}>{title}</h2>
      {lead !== undefined && <p className={styles.sectionLead}>{lead}</p>}
    </>
  );
}
