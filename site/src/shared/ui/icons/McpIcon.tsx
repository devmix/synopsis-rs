import type {ReactNode} from 'react';

import {iconProps} from './iconProps';

export function McpIcon(): ReactNode {
  return (
    <svg {...iconProps}>
      <rect x="4" y="4" width="16" height="7" rx="1.5" />
      <rect x="4" y="13" width="16" height="7" rx="1.5" />
      <path d="M8 7.5h.01" />
      <path d="M8 16.5h.01" />
    </svg>
  );
}
