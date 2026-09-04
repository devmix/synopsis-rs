import {useEffect, useRef} from 'react';
import type {TermLine} from '../types';

import {CURSOR_CLASS, TLINE_CLASS, TONE_CLASS} from '../../ui/terminal/terminal';

export function useTerminalTypewriter(lines: readonly TermLine[]) {
  const bodyRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const body = bodyRef.current;
    if (body === null) return;

    const reducedMotion = window.matchMedia(
      '(prefers-reduced-motion: reduce)',
    ).matches;

    const timeouts: number[] = [];
    const intervals: number[] = [];
    let cancelled = false;
    const after = (fn: () => void, ms: number): void => {
      timeouts.push(window.setTimeout(() => {
        if (!cancelled) fn();
      }, ms));
    };

    body.textContent = '';
    const cursor = document.createElement('span');
    cursor.className = CURSOR_CLASS;

    const makeLine = (line: TermLine): HTMLElement => {
      const row = document.createElement('div');
      row.className = TLINE_CLASS;
      const span = document.createElement('span');
      span.className = TONE_CLASS[line.tone];
      row.appendChild(span);
      body.appendChild(row);
      return span;
    };

    if (reducedMotion) {
      for (const line of lines) {
        makeLine(line).textContent = line.text;
      }
      body.lastElementChild?.appendChild(cursor);
    } else {
      let lineIndex = 0;
      const typeNextLine = (): void => {
        if (lineIndex >= lines.length) return;
        const line = lines[lineIndex];
        const span = makeLine(line);
        body.lastElementChild?.appendChild(cursor);
        let charIndex = 0;
        intervals.push(
          window.setInterval(() => {
            if (cancelled) return;
            charIndex += 1;
            span.textContent = line.text.slice(0, charIndex);
            if (charIndex >= line.text.length) {
              window.clearInterval(intervals[intervals.length - 1]);
              lineIndex += 1;
              if (lineIndex < lines.length) {
                after(typeNextLine, line.tone === 'cmd' ? 420 : 240);
              }
            }
          }, line.tone === 'cmd' ? 28 : 11),
        );
      };
      after(typeNextLine, 700);
    }

    return () => {
      cancelled = true;
      timeouts.forEach((id) => window.clearTimeout(id));
      intervals.forEach((id) => window.clearInterval(id));
    };
  }, [lines]);

  return bodyRef;
}
