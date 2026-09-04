import type {
  Feature,
  FlowStep,
  McpTool,
  PipelineStage,
  TermLine,
} from './types';

import {BinaryIcon, DomainIcon, GraphIcon, LinkIcon, McpIcon, SearchIcon} from '../ui/icons';

export const GITHUB_URL = 'https://github.com/devmix/synopsis';
export const GITHUB_RELEASES_URL = `${GITHUB_URL}/releases`;

export const TERMINAL_LINES: readonly TermLine[] = [
  {text: '$ ./synopsis onnx-runtime install', tone: 'cmd'},
  {text: '✓ onnxruntime — cpu · linux/amd64', tone: 'ok'},
  {text: '$ ./synopsis sync', tone: 'cmd'},
  {text: '✓ model bge-small-en-v1.5 · 384 dims (auto-downloaded)', tone: 'ok'},
  {text: '✓ 1,284 docs · 12,902 chunks embedded (onnx)', tone: 'ok'},
  {text: '✓ 3,411 entities · 9,027 facts · graph loaded', tone: 'ok'},
  {text: '$ ./synopsis serve', tone: 'cmd'},
  {text: '✓ mcp server → :8080/sse', tone: 'ok'},
  {text: '$ search {"query":"vacation policy","domain":"hr"}', tone: 'cmd'},
  {text: '✓ 10 chunks · bm25+vector · rrf', tone: 'hl'},
];

export const TICKER_ITEMS: readonly string[] = [
  'GoLang',
  'Cross-Platform',
  'MCP / HTTP / SSE',
  'LLM',
  'SQLite',
  'FTS5',
  'ONNX',
  'CEL',
  'Lexical / Semantic : Hybrid Search',
  'NER',
  'Ontology',
  'Knowledge Graph',
  'Single binary',
];

export const PIPELINE_STAGES: readonly PipelineStage[] = [
  {
    num: '01',
    title: 'Clean data',
    desc: 'Documents arrive normalized and tagged with domains. Synopsis does not clean or deduplicate — messy data is a stage you run upstream.',
    meta: '.md · .json · .html',
    chips: ['Markdown', 'JSON', 'HTML'],
  },
  {
    num: '02',
    title: 'Unified format',
    desc: 'A format-specific parser turns every file into the same structured document — text, metadata, and source tags preserved.',
    meta: 'sources declared in ontology xml',
    chips: ['Parser', 'Metadata', 'Source tags'],
  },
  {
    num: '03',
    title: 'Processing',
    desc: 'sync runs parse → chunk → embed → NER → entities → facts into a knowledge graph plus a hybrid search index, then links entities across domains.',
    meta: './synopsis sync · one sqlite file',
    chips: ['Chunking', 'Embeddings', 'NER', 'Knowledge graph', 'Hybrid index'],
  },
  {
    num: '04',
    title: 'MCP access',
    desc: 'serve exposes 12 tools over HTTP/SSE. Agents retrieve chunks, entities, facts, and graph neighbors — every answer with full provenance.',
    meta: 'GET /sse · POST /message',
    chips: ['HTTP/SSE', '12 tools', 'Provenance'],
    final: true,
  },
];

export const FEATURES: readonly Feature[] = [
  {
    num: '/01',
    icon: <SearchIcon />,
    name: 'Hybrid Search',
    desc: 'Lexical (BM25) and semantic (vector) retrieval run in parallel, fused with Reciprocal Rank Fusion and reranked with authority and freshness boosts.',
    tags: ['FTS5/BM25', 'sqlite-vec', 'RRF'],
  },
  {
    num: '/02',
    icon: <GraphIcon />,
    name: 'Knowledge Graph',
    desc: 'Entities, facts, and relations load into memory at startup — O(1) lookups, BFS traversal, and a complete dossier per entity, facts and quotes included.',
    tags: ['in-memory', 'BFS', 'dossiers'],
  },
  {
    num: '/03',
    icon: <BinaryIcon />,
    name: 'Zero-Infrastructure',
    desc: 'One Go binary, one SQLite file. No Python, no Postgres, no Docker — embeddings are computed locally on CPU with ONNX Runtime.',
    tags: ['single binary', 'CGO', 'SQLite'],
  },
  {
    num: '/04',
    icon: <McpIcon />,
    name: 'MCP-Native',
    desc: 'Twelve self-documenting tools over HTTP/SSE, built for Claude Desktop, Cursor, and any MCP client — not for human browsing.',
    tags: ['HTTP/SSE', '12 tools', 'agents'],
  },
  {
    num: '/05',
    icon: <LinkIcon />,
    name: 'Cross-Domain Linking',
    desc: 'The same entity across HR, IT, and Product is linked during ingestion — every link carries its method, confidence, and evidence.',
    tags: ['CEL / equals / llm', 'provenance'],
  },
  {
    num: '/06',
    icon: <DomainIcon />,
    name: 'Domain-Based Config',
    desc: 'Sources, ontologies, and extraction methods are declared per domain in XML — HR, IT, Product, and your own vocabulary.',
    tags: ['ontology', 'domains', 'sources'],
  },
];

export const INGESTION_STEPS: readonly FlowStep[] = [
  {
    title: 'Parse & chunk',
    desc: 'Markdown, JSON, MediaWiki, HTML, and unstructured text — header-based or fixed-size chunking with overlap.',
  },
  {
    title: 'Embed & store',
    desc: 'ONNX Runtime generates L2-normalized vectors per chunk in sqlite-vec; text goes to FTS5 via sync triggers.',
  },
  {
    title: 'NER & entities',
    desc: 'Regex ontologies, offline statistical NER, or LLM extraction. Entities are deduplicated and linked to their chunks.',
  },
  {
    title: 'Facts & graph',
    desc: 'Subject–predicate–object facts gain confidence scores and source quotes; the graph and the hybrid index are built in the same pass.',
  },
];

export const SEARCH_STEPS: readonly FlowStep[] = [
  {
    title: 'Parallel search',
    desc: 'A tools/call search runs BM25 (FTS5) and cosine KNN (sqlite-vec) concurrently over the whole knowledge base.',
  },
  {
    title: 'RRF fusion',
    desc: 'Reciprocal Rank Fusion merges both rankings — no score normalization between lexicon and cosine.',
  },
  {
    title: 'Enrich & rerank',
    desc: 'Each hit gets document metadata and its entities attached; authority and freshness boosts reorder the tail.',
  },
  {
    title: 'Graph expansion',
    desc: 'BFS from the result entities pulls in related edges and approved facts before the answer leaves the server.',
  },
];

export const QUISTEPS: readonly {cmd: string; title: string; desc: string}[] = [
  {
    cmd: './synopsis onnx-runtime install',
    title: 'Install',
    desc: 'One-time setup: installs the ONNX Runtime library for fully local embeddings. The default model (bge-small-en-v1.5) auto-downloads on first sync.',
  },
  {
    cmd: './synopsis sync',
    title: 'Sync',
    desc: 'Drop cleaned documents into documents/ — parsing, chunking, local ONNX embeddings, NER, facts, graph, and index land in one data/knowledge.db.',
  },
  {
    cmd: './synopsis serve',
    title: 'Serve',
    desc: 'The MCP server starts on :8080 with /sse, /message, and /health — initial sync plus a file watcher keep it fresh.',
  },
];

export const QUICKSTART_LINES: readonly TermLine[] = [
  {text: '$ ./synopsis sync', tone: 'cmd'},
  {text: '✓ 1,284 parsed · 12,902 embedded (onnx · bge-small)', tone: 'ok'},
  {text: '✓ 3,411 entities · 9,027 facts · graph + fts5 ready', tone: 'ok'},
  {text: '$ ./synopsis serve', tone: 'cmd'},
  {text: '✓ mcp server → http://localhost:8080/sse', tone: 'ok'},
  {text: '$ curl -s http://localhost:8080/health', tone: 'cmd'},
  {text: '{"status":"ok"}', tone: 'hl'},
];

export const MCP_TOOLS_BASE = '/docs/reference/mcp-tools';

export const MCP_TOOLS: readonly McpTool[] = [
  {name: 'search', cat: 'search', href: `${MCP_TOOLS_BASE}/search`, desc: 'Hybrid lexical (FTS5/BM25) + semantic (vector) search, fused with RRF — the primary read path for agents.'},
  {name: 'catalog_overview', cat: 'catalog', href: `${MCP_TOOLS_BASE}/catalog-overview`, desc: 'Aggregate knowledge-base stats — documents, chunks, entities, facts, distributions, graph size.'},
  {name: 'catalog_documents', cat: 'catalog', href: `${MCP_TOOLS_BASE}/catalog-documents`, desc: 'List ingested documents with cursor pagination and domain, type, and name filters.'},
  {name: 'catalog_entities', cat: 'catalog', href: `${MCP_TOOLS_BASE}/catalog-entities`, desc: 'List knowledge-graph entities with pagination and type, domain, and name filters.'},
  {name: 'search_entities_by_type', cat: 'catalog', href: `${MCP_TOOLS_BASE}/search-entities-by-type`, desc: 'All entities of one type, paginated, with an optional domain filter.'},
  {name: 'search_facts', cat: 'catalog', href: `${MCP_TOOLS_BASE}/search-facts`, desc: 'Search facts by predicate, entity name, status, and domain — triples with resolved entities.'},
  {name: 'get_document_context', cat: 'retrieval', href: `${MCP_TOOLS_BASE}/get-document-context`, desc: 'Full context of one document — metadata, chunks with offsets, entities, approved fact IDs.'},
  {name: 'get_chunk_by_id', cat: 'retrieval', href: `${MCP_TOOLS_BASE}/get-chunk-by-id`, desc: 'A single chunk with full text, offsets, parent document, and associated entities.'},
  {name: 'get_fact_by_id', cat: 'retrieval', href: `${MCP_TOOLS_BASE}/get-fact-by-id`, desc: 'One fact triple with subject/object details, status, validity, and source quotes.'},
  {name: 'get_entity_dossier', cat: 'graph', href: `${MCP_TOOLS_BASE}/get-entity-dossier`, desc: 'Complete dossier — approved facts with quotes, source documents, neighbors, cross-domain links.'},
  {name: 'get_entity_relations', cat: 'graph', href: `${MCP_TOOLS_BASE}/get-entity-relations`, desc: 'BFS traversal of the in-memory graph — nodes, edges, relation types, counters.'},
  {name: 'get_entity_links', cat: 'graph', href: `${MCP_TOOLS_BASE}/get-entity-links`, desc: 'Cross-domain links with method, confidence, and evidence — straight from the database.'},
];

export const USE_CASES: readonly {tag: string; title: string; desc: string; chips: readonly string[]}[] = [
  {
    tag: 'HR',
    title: 'People operations',
    desc: 'Vacation, hiring, benefits — ask “how does the policy apply to contractors?” and get the answer with the exact source quote.',
    chips: ['policy', 'role', 'process'],
  },
  {
    tag: 'IT',
    title: 'Infrastructure ops',
    desc: 'Runbooks, incidents, change records — find the last fix for a service outage across the wiki and ticket history.',
    chips: ['service', 'incident', 'runbook'],
  },
  {
    tag: 'Product',
    title: 'Product teams',
    desc: 'Specs, roadmaps, feedback — keep features, decisions, and owners consistent in one queryable place.',
    chips: ['feature', 'decision', 'release'],
  },
  {
    tag: 'Engineering',
    title: 'Engineering',
    desc: 'Design docs, ADRs, post-mortems — the same system or person linked across code, prose, and tickets.',
    chips: ['adr', 'system', 'post-mortem'],
  },
];

export const TECH_ROWS: readonly {name: string; role: string; why: string}[] = [
  {name: 'Go 1.25 (CGO)', role: 'Single self-contained binary', why: 'One artifact to ship — SQLite FTS5 and sqlite-vec are compiled in at build time.'},
  {name: 'SQLite', role: 'Storage + FTS5 + sqlite-vec', why: 'Zero infrastructure; WAL mode keeps serving reads while ingestion writes.'},
  {name: 'ONNX Runtime', role: 'Local embeddings on CPU', why: 'No Python, no API keys — INT8-quantized models run fully offline.'},
  {name: 'MCP over HTTP/SSE', role: 'Agent access layer', why: 'Native integration with Claude Desktop and Cursor; declarative, self-documenting tools.'},
  {name: 'Zig 0.14+', role: 'Cross-compilation', why: 'Linux, macOS, and Windows binaries from a single source tree.'},
];

