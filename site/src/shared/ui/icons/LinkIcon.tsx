import type {ReactNode} from 'react';

import {iconProps} from './iconProps';

export function LinkIcon(): ReactNode {
  return (
    <svg {...iconProps}>
      <path d="m9 15 6-6" />
      <path d="M11 6.5 13.5 4a3.54 3.54 0 0 1 5 5L16 11.5" />
      <path d="M13 17.5 10.5 20a3.54 3.54 0 0 1-5-5L8 12.5" />
    </svg>
  );
}
