/**
 * Cookie consent banner (GDPR / 152-FZ).
 *
 * Injects a fixed bottom bar on first visit. On Accept click the consent flag
 * is persisted to localStorage and the banner animates out of view before being
 * removed from the DOM. Follows the navbar-scroll.ts pattern: vanilla TS, no
 * React, registered via clientModules in docusaurus.config.ts.
 */
import type {ClientModule} from '@docusaurus/types';

const STORAGE_KEY = 'synopsis-cookie-consent';
const COOKIE_POLICY_URL = 'https://tekblueprint.org/legal/use-cookies/';

let bannerInjected = false;

function createBanner(): HTMLElement {
    const banner = document.createElement('div');
    banner.className = 'cookie-banner';
    banner.setAttribute('role', 'dialog');
    banner.setAttribute('aria-live', 'polite');
    banner.setAttribute('aria-label', 'Cookie consent');
    banner.innerHTML = `
        <p class="cookie-banner__text">
            We use cookies to improve your experience and analyze site traffic. By continuing to browse, you agree to our <a href="${COOKIE_POLICY_URL}" target="_self" rel="noopener noreferrer">Cookie Policy</a>.
        </p>
        <button type="button" class="cookie-banner__accept">Accept</button>
    `;

    const acceptBtn = banner.querySelector<HTMLButtonElement>('.cookie-banner__accept');

    /** Remove the banner from DOM after its exit transition finishes. */
    function removeBanner(): void {
        if (banner.parentNode) {
            banner.parentNode.removeChild(banner);
        }
    }

    if (acceptBtn) {
        acceptBtn.addEventListener('click', () => {
            localStorage.setItem(STORAGE_KEY, '1');
            banner.classList.add('cookie-banner--hidden');
            // Use transitionend so JS stays in sync with CSS --dur-base.
            const onTransitionEnd = () => {
                banner.removeEventListener('transitionend', onTransitionEnd);
                removeBanner();
            };
            banner.addEventListener('transitionend', onTransitionEnd);
        });

        /** Escape key: dismiss without persisting consent (reappears next session). */
        function handleKeydown(e: KeyboardEvent): void {
            if (e.key === 'Escape') {
                banner.classList.add('cookie-banner--hidden');
                const onTransitionEnd = () => {
                    banner.removeEventListener('transitionend', onTransitionEnd);
                    banner.removeEventListener('keydown', handleKeydown);
                    removeBanner();
                };
                banner.addEventListener('transitionend', onTransitionEnd);
            }
        }

        acceptBtn.addEventListener('keydown', (e: KeyboardEvent) => {
            if (e.key === 'Escape') {
                e.stopPropagation();
                handleKeydown(e);
            }
        });
    }

    return banner;
}

const clientModule: ClientModule = {
    onRouteDidUpdate(): void {
        // Guard: browser only, inject once per session.
        if (bannerInjected || typeof document === 'undefined') {
            return;
        }

        const consented = localStorage.getItem(STORAGE_KEY);
        if (consented) {
            bannerInjected = true;
            return;
        }

        const banner = createBanner();
        document.body.appendChild(banner);

        // Focus the Accept button so keyboard users can act immediately (WCAG 2.4.3).
        const acceptBtn = banner.querySelector<HTMLButtonElement>('.cookie-banner__accept');
        if (acceptBtn) {
            acceptBtn.focus();
        }

        bannerInjected = true;
    },
};

export default clientModule;
