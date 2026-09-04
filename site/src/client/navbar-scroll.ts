/**
 * Navbar scroll state (NAVBAR-V5-STYLE).
 *
 * Toggles `.navbar--scrolled` on the theme navbar once the page is scrolled
 * past 12px. The bar renders transparent at top and gains a translucent blur
 * + hairline border in the scrolled state — both states are styled in
 * src/css/custom.css (no swizzling).
 *
 * Docusaurus client modules expose no unmount hook, so the scroll listener is
 * attached exactly once per page load (module-level guard) instead of being
 * re-attached on every route update.
 */
import type {ClientModule} from '@docusaurus/types';

const SCROLL_THRESHOLD_PX = 12;

let listenerAttached = false;
let framePending = false;

function applyScrollState(): void {
    const navbar = document.querySelector<HTMLElement>('.navbar');
    if (navbar) {
        navbar.classList.toggle(
            'navbar--scrolled',
            window.scrollY > SCROLL_THRESHOLD_PX,
        );
    }
}

const clientModule: ClientModule = {
    onRouteDidUpdate(): void {
        // Re-evaluate immediately — the browser resets scroll position on navigation.
        applyScrollState();

        if (listenerAttached || typeof window === 'undefined') {
            return;
        }
        listenerAttached = true;

        const handleScroll = (): void => {
            // Throttle to one class toggle per animation frame.
            if (framePending) {
                return;
            }
            framePending = true;
            requestAnimationFrame(() => {
                applyScrollState();
                framePending = false;
            });
        };

        window.addEventListener('scroll', handleScroll, {passive: true});
    },
};

export default clientModule;
