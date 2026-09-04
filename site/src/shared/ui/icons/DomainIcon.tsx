import type {ReactNode} from 'react';

import {iconProps} from './iconProps';

export function DomainIcon(): ReactNode {
  return (
    <svg {...iconProps}>
      <path d="M5 4v6.5" />
      <path d="M5 15.5V20" />
      <path d="M12 4v2.5" />
      <path d="M12 11.5V20" />
      <path d="M19 4v10.5" />
      <path d="M19 19v1" />
      <circle cx="5" cy="13" r="2" />
      <circle cx="12" cy="9" r="2" />
      <circle cx="19" cy="16.5" r="2" />
    </svg>
  );
}
