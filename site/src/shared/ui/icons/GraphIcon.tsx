import type {ReactNode} from 'react';

import {iconProps} from './iconProps';

export function GraphIcon(): ReactNode {
  return (
    <svg {...iconProps}>
      <circle cx="5" cy="6" r="2.5" />
      <circle cx="19" cy="6" r="2.5" />
      <circle cx="12" cy="18" r="2.5" />
      <path d="M7.5 6h9" />
      <path d="M6.2 8.2 10.8 15.9" />
      <path d="M17.8 8.2 13.2 15.9" />
    </svg>
  );
}
