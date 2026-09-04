import {useEffect} from 'react';

export function useRevealOnScroll(): void {
  useEffect(() => {
    const elements = Array.from(
      document.querySelectorAll<HTMLElement>('[data-reveal]'),
    );
    if (elements.length === 0) return;
    // Reveal state lives in the data-revealed attribute, not a class: React
    // rewrites className from its vdom on every re-render (e.g. the accordion
    // toggling capOpen), which would wipe a manually-added class and hide
    // already-revealed content. Attributes absent from JSX props are left
    // untouched by React, so data-revealed survives re-renders.
    const markRevealed = (el: Element): void => {
      el.setAttribute('data-revealed', 'true');
    };
    if (typeof IntersectionObserver === 'undefined') {
      elements.forEach(markRevealed);
      return;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) {
            markRevealed(entry.target);
            observer.unobserve(entry.target);
          }
        }
      },
      {threshold: 0.12},
    );
    elements.forEach((el) => observer.observe(el));
    return () => observer.disconnect();
  }, []);
}
