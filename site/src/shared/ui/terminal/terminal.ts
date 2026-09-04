import type {Tone} from '../../lib/types';

import styles from './terminal.module.css';

export const TONE_CLASS: Record<Tone, string> = {
  cmd: styles.tCmd,
  ok: styles.tOk,
  dim: styles.tDim,
  hl: styles.tHl,
};

export const TLINE_CLASS = styles.tline;

export const CURSOR_CLASS = styles.cursor;
