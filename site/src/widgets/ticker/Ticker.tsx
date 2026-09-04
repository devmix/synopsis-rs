import type {ReactNode} from 'react';

import {TICKER_ITEMS} from '../../shared/lib/data';

import styles from './Ticker.module.css';

export function Ticker(): ReactNode {
  return (
    <div className={styles.ticker} aria-hidden="true">
      <div className={styles.tickerTrack}>
        {[0, 1].map((group) => (
          <div key={group} className={styles.tickerGroup}>
            {TICKER_ITEMS.map((item) => (
              <span key={item} className={styles.tickItem}>
                {item}
              </span>
            ))}
          </div>
        ))}
      </div>
    </div>
  );
}
