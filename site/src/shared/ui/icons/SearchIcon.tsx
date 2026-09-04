import type {ReactNode} from 'react';

import {iconProps} from './iconProps';

export function SearchIcon(): ReactNode {
  return (
    <svg {...iconProps}>
      <circle cx="11" cy="11" r="7" />
      <path d="m21 21-4.3-4.3" />
      <path d="M8 11h6" />
    </svg>
  );
}
