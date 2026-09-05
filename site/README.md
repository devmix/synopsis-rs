# Synopsis Docs Site

The documentation site for [Synopsis](https://github.com/devmix/synopsis-rs) — a local RAG + knowledge-graph MCP server in Rust. Built with [Docusaurus 3.10.2](https://docusaurus.io/).

## Prerequisites

- Node.js >= 20

## Install

```bash
npm install
```

## Local Development

```bash
npm run start
```

Starts a local development server with live reload.

## Build

```bash
npm run build
```

Generates static content into the `build/` directory.

## Preview the Build

```bash
npm run serve
```

Serves the `build/` output locally for preview.

## Deployment

Deployment is MANUAL: copy the `build/` directory to the hosting at
https://synopsis-memex.tekblueprint.org.

This site is NOT part of the Rust workspace build and is NOT built in CI.
