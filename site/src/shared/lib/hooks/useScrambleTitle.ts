import {useEffect, useRef} from 'react';

const SCRAMBLE_CHARS = '█▓▒░<>/\\{}[]#*+=~';
const STEP_MS = 34;
const BASE_DELAY_MS = 200;
const STAGGER_MS = 380;

// Scramble-decode for the hero title: every <span data-text> line inside the
// heading resolves from random glyphs to its final text, lines staggered.
// Final text stays in the markup (SEO + no-JS); the hook only re-animates it
// on the client. With prefers-reduced-motion the markup is left as-is.
export function useScrambleTitle() {
  const titleRef = useRef<HTMLHeadingElement>(null);

  useEffect(() => {
    const title = titleRef.current;
    if (title === null) return;

    const targets = Array.from(title.querySelectorAll<HTMLElement>('[data-text]'));
    if (targets.length === 0) return;

    if (
      window.matchMedia('(prefers-reduced-motion: reduce)').matches
    ) {
      return;
    }

    let cancelled = false;
    const timeouts: number[] = [];
    const intervals: number[] = [];

    targets.forEach((el, i) => {
      const text = el.dataset.text ?? '';
      el.textContent = '';
      timeouts.push(
        window.setTimeout(() => {
          if (cancelled) return;
          let step = 0;
          intervals.push(
            window.setInterval(() => {
              if (cancelled) return;
              step += 1;
              let out = '';
              for (let j = 0; j < text.length; j += 1) {
                if (j < step) out += text[j];
                else if (text[j] === ' ') out += ' ';
                else
                  out +=
                    SCRAMBLE_CHARS[
                      Math.floor(Math.random() * SCRAMBLE_CHARS.length)
                    ];
              }
              el.textContent = out;
              if (step >= text.length) {
                window.clearInterval(intervals[intervals.length - 1]);
              }
            }, STEP_MS),
          );
        }, BASE_DELAY_MS + i * STAGGER_MS),
      );
    });

    return () => {
      cancelled = true;
      timeouts.forEach((id) => window.clearTimeout(id));
      intervals.forEach((id) => window.clearInterval(id));
    };
  }, []);

  return titleRef;
}
