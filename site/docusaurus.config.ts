import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

// This runs in Node.js - Don't use client-side code here (browser APIs, JSX...)

const config: Config = {
    title: 'synopsis[memex]',
    tagline: 'Zero-Infrastructure RAG & Knowledge Graph',
    favicon: 'img/favicon.ico',

    future: {
        v4: true,
    },

    markdown: {
        mermaid: true,
    },

    // Client modules.
    clientModules: [
        './src/client/navbar-scroll',   // transparent-at-top → blurred after 12px scroll
        './src/client/cookie-banner',   // GDPR / 152-FZ cookie notice (Accept-only)
    ],

    url: 'https://devmix.github.io',
    baseUrl: '/',

    organizationName: 'devmix',
    projectName: 'synopsis',

    onBrokenLinks: 'throw',

    headTags: [
        {
            tagName: 'link',
            attributes: {rel: 'preconnect', href: 'https://fonts.googleapis.com'},
        },
        {
            tagName: 'link',
            attributes: {
                rel: 'preconnect',
                href: 'https://fonts.gstatic.com',
                crossorigin: 'anonymous',
            },
        },
        // Yandex.Metrika counter
        {
            tagName: 'script',
            attributes: {},
            innerHTML: `(function(m,e,t,r,i,k,a){m[i]=m[i]||function(){(m[i].a=m[i].a||[]).push(arguments)};m[i].l=1*new Date();for(var j=0;j<document.scripts.length;j++){if(document.scripts[j].src===r){return;}}k=e.createElement(t),a=e.getElementsByTagName(t)[0],k.async=1,k.src=r,a.parentNode.insertBefore(k,a)})(window,document,'script','https://mc.yandex.ru/metrika/tag.js?id=104146397','ym');ym(104146397,'init',{ssr:true,webvisor:true,clickmap:true,accurateTrackBounce:true,trackLinks:true});`,
        },
    ],

    stylesheets: [
        'https://fonts.googleapis.com/css2?family=JetBrains+Mono:wght@400;500&family=Manrope:wght@400;500;600&family=Space+Grotesk:wght@500;600;700&display=swap',
        // Global design tokens (raw CSS, not a module). Lives in static/ because
        // files under src/ are only bundled when imported and are never served as
        // raw assets — a './src/css/...' href here would 404/fall back to the SPA.
        // Absolute URL derived from baseUrl so it resolves on every route (dev + prod).
    ],

    presets: [
        [
            'classic',
            {
                docs: {
                    sidebarPath: './sidebars.ts',
                    editUrl: 'https://github.com/devmix/synopsis/tree/main/docs/website/',
                },
                blog: false,
                theme: {
                    customCss: './src/css/custom.css',
                },
            } satisfies Preset.Options,
        ],
    ],

    themes: [
        '@docusaurus/theme-mermaid',
        [
            '@easyops-cn/docusaurus-search-local',
            {
                hashed: true,
                indexBlog: false,
                indexPages: false,
                language: ['en'],
                highlightSearchTermsOnTargetPage: true,
                docsRouteBasePath: '/docs',
                searchBarShortcutKeymap: 'mod+k',
            },
        ],
    ],

    themeConfig: {
        image: 'img/docusaurus-social-card.jpg',
        colorMode: {
            defaultMode: 'dark',
            disableSwitch: false,
            respectPrefersColorScheme: false,
        },
        mermaid: {
            theme: {
                light: 'neutral',
                dark: 'dark',
            },
        },
        navbar: {
            // Text wordmark (mono) + CSS accent dot (::before), per .mockups/index-v5.html.
            title: 'synopsis_',
            items: [
                {
                    type: 'docSidebar',
                    sidebarId: 'tutorialSidebar',
                    position: 'left',
                    label: 'Docs',
                },
                // /docs/reference has no index page → point at the flagship reference doc.
                // (Its folder's index.mdx keeps "/index" in its id on Docusaurus 3.10.)
                {
                    type: 'doc',
                    docId: 'reference/mcp-tools/index',
                    position: 'left',
                    label: 'Reference',
                },
                {
                    type: 'doc',
                    docId: 'roadmap',
                    position: 'left',
                    label: 'Roadmap',
                },
                {
                    href: 'https://github.com/devmix/synopsis',
                    label: 'GitHub',
                    position: 'right',
                },
            ],
        },
        footer: {
            style: 'dark',
            links: [
                {
                    className: 'footer-col-brand',
                    title: ' ', /* non-empty to satisfy multi-column validator; hidden via CSS */
                    items: [
                        {
                            html: '<span class="foot-logo"><span class="foot-logo-dot"></span>synopsis_</span><p class="foot-brand">Zero-Infrastructure RAG &amp; Knowledge Graph</p>',
                        },
                    ],
                },
                {
                    title: 'Documentation',
                    items: [
                        {
                            label: 'Introduction',
                            to: '/docs/intro',
                        },
                        {
                            label: 'Quickstart',
                            to: '/docs/quickstart',
                        },
                        {
                            label: 'The Pipeline',
                            to: '/docs/concepts/pipeline',
                        },
                        {
                            label: 'MCP Tools',
                            to: '/docs/reference/mcp-tools',
                        },
                    ],
                },
                {
                    title: 'Reference',
                    items: [
                        {
                            label: 'CLI',
                            to: '/docs/reference/cli',
                        },
                        {
                            label: 'Config Schema',
                            to: '/docs/reference/config-schema',
                        },
                        {
                            label: 'Database Schema',
                            to: '/docs/reference/database-schema',
                        },
                        {
                            label: 'Roadmap',
                            to: '/docs/roadmap',
                        },
                    ],
                },
                {
                    title: 'Community',
                    items: [
                        {
                            label: 'GitHub',
                            href: 'https://github.com/devmix/synopsis',
                        },
                    ],
                },
                {
                    title: 'Legal',
                    items: [
                        {
                            label: 'Privacy Policy',
                            href: 'https://tekblueprint.org/legal/privacy-policy/',
                        },
                        {
                            label: 'Terms of Service',
                            href: 'https://tekblueprint.org/legal/terms-of-service/',
                        },
                        {
                            label: 'Cookie Policy',
                            href: 'https://tekblueprint.org/legal/use-cookies/',
                        },
                    ],
                },
            ],
            copyright: `Copyright © ${new Date().getFullYear()} synopsis[memex] — structured information for AI agents via MCP.`,
        },
        prism: {
            theme: prismThemes.github,
            darkTheme: prismThemes.dracula,
        },
    } satisfies Preset.ThemeConfig,
};

export default config;
