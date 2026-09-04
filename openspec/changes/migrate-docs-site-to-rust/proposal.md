# Proposal: migrate-docs-site-to-rust

## Why

The project's documentation site was built for the old Go implementation and
describes functionality that no longer exists or has been replaced: the `sync`
subcommand (removed), CGO-based builds (replaced by bundled in-tree SQLite),
the sqlite-vec/vec0 vector store (replaced by usearch), a single SQLite
database (now two: per-dataset knowledge DB + global cache DB), GitHub Actions
CI (now Gitea-compatible), and Docker (no longer a supported path). The site
also lives outside this repository, in the old project's directory
(`/mnt/local/secure/projects/devmix/synopsis/site`). It must be moved into this
repository and fully updated to document the Rust implementation.

## What

- **Move the Docusaurus 3.10.2 site into `site/`** — verbatim copy of the
  scaffold (framework config, landing page with its 11 widgets, shared UI kit,
  CSS design tokens, local search + mermaid plugins), excluding
  `node_modules/`, `build/`, `.docusaurus/`, and the empty `scripts/` dir.
  Replace the stale `docs/website/` entries in the root `.gitignore` with
  `site/` entries.
- **Full content rewrite of all docs (~3500 lines of MDX) and the 11 landing
  widgets** against the 14 contract specs (`openspec/specs/*/spec.md`) plus
  `README.md`, `AGENTS.md`, `docs/adr/`, and `.github/workflows/` — the
  sources of truth for the Rust implementation.
- **8 new pages** for functionality that exists in the Rust implementation but
  not in the old site: concepts — `workspace-layout`, `job-queue`,
  `vector-rebuild`, `cache-db`, `mcp-transport`; developer — `gitea-releases`,
  `adrs`, `openspec-workflow`.
- **Delete `docs/developer/docker.mdx`** (Docker was a Go-era concern; the
  Rust binary builds natively — musl static, zigbuild cross-builds).
- **Update `docusaurus.config.ts`**: drop the GitHub Pages deploy fields, set
  `url` to `https://synopsis-memex.tekblueprint.org`, point navbar/footer/
  editUrl links at `https://github.com/devmix/synopsis-rs`, keep
  Yandex.Metrika, fonts, cookie banner, local search, and mermaid.
- **Rewrite `site/README.md`** (Docusaurus boilerplate → local build/deploy
  instructions) and delete the template leftover `src/pages/markdown-page.mdx`.

**Frozen contracts:** NO behavior change. This change documents the frozen
contracts (mcp-contract, cli-surface, data-schema, config-format) without
modifying them. No Rust code, `Cargo.toml`, migrations, or `openspec/specs/**`
are touched. Parity is confirmed by machine gates (`npm run typecheck` +
`npm run build` with `onBrokenLinks: 'throw'` on every task) and by a final
content-parity audit against all 14 spec files (task 1.20).

## Non-goals

- **NOT Rust code** — `crates/**`, `Cargo.toml`, `Cargo.lock`, `migrations/**`
  are untouched.
- **NOT the frozen contract specs** — `openspec/specs/**` are the reference,
  not the target.
- **NOT site CI/CD** — no site build in `.github/workflows/**` (user decision:
  deployment is manual, to the user's own site).
- **NOT deployment automation** — no `npm run deploy` config; GitHub Pages
  fields are removed, not re-pointed.
- **NOT i18n** — English only (the old site has no i18n; user decision).
- **NOT a redesign** — the landing page design, widgets, and CSS design tokens
  are preserved; only content changes.
- **NOT the old project directory** — `/mnt/local/secure/projects/devmix/
  synopsis/site` stays as-is (it is the copy source, never modified).
