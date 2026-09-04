import type {ReactNode} from 'react';

import {iconProps} from './iconProps';

export function BinaryIcon(): ReactNode {
  return (
    <svg {...iconProps}>
      <rect x="3.5" y="5" width="17" height="14" rx="2" />
      <path d="m7.5 10 3 3-3 3" />
      <path d="M13 16.5h4" />
    </svg>
  );
}
