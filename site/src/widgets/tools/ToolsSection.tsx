import Link from '@docusaurus/Link';
import type {ReactNode} from 'react';

import type {McpTool} from '../../shared/lib/types';

import styles from './ToolsSection.module.css';

const MCP_TOOLS_BASE = '/docs/reference/mcp-tools';

const MCP_TOOLS: readonly McpTool[] = [
  {name: 'search', cat: 'search', href: `${MCP_TOOLS_BASE}/search`, desc: 'Hybrid lexical (FTS5/BM25) + semantic (usearch HNSW) search, fused with RRF and reranked by recency and authority — the primary read path for agents.'},
  {name: 'catalog_overview', cat: 'catalog', href: `${MCP_TOOLS_BASE}/catalog-overview`, desc: 'Aggregate knowledge-base stats — documents, chunks, entities, facts, distributions, graph size.'},
  {name: 'catalog_documents', cat: 'catalog', href: `${MCP_TOOLS_BASE}/catalog-documents`, desc: 'List ingested documents with cursor pagination and domain, type, and name filters.'},
  {name: 'catalog_entities', cat: 'catalog', href: `${MCP_TOOLS_BASE}/catalog-entities`, desc: 'List knowledge-graph entities with pagination and type, domain, and name filters.'},
  {name: 'search_entities_by_type', cat: 'catalog', href: `${MCP_TOOLS_BASE}/search-entities-by-type`, desc: 'All entities of one type, paginated, with an optional domain filter.'},
  {name: 'search_facts', cat: 'catalog', href: `${MCP_TOOLS_BASE}/search-facts`, desc: 'Search approved facts by predicate, entity name, status, and domain — triples with resolved entities.'},
  {name: 'get_document_context', cat: 'retrieval', href: `${MCP_TOOLS_BASE}/get-document-context`, desc: 'Full context of one document — metadata, chunks with offsets, entities, approved fact IDs.'},
  {name: 'get_chunk_by_id', cat: 'retrieval', href: `${MCP_TOOLS_BASE}/get-chunk-by-id`, desc: 'A single chunk with full text, offsets, parent document, and associated entities.'},
  {name: 'get_fact_by_id', cat: 'retrieval', href: `${MCP_TOOLS_BASE}/get-fact-by-id`, desc: 'One fact triple with subject/object details, status, validity, and source quotes.'},
  {name: 'get_entity_dossier', cat: 'graph', href: `${MCP_TOOLS_BASE}/get-entity-dossier`, desc: 'Complete dossier — approved facts with quotes, source documents, neighbors, cross-domain links.'},
  {name: 'get_entity_relations', cat: 'graph', href: `${MCP_TOOLS_BASE}/get-entity-relations`, desc: 'Traversal of the petgraph index — nodes, edges, relation types, counters.'},
  {name: 'get_entity_links', cat: 'graph', href: `${MCP_TOOLS_BASE}/get-entity-links`, desc: 'Cross-domain links with method, confidence, and evidence — straight from the database.'},
];

export function ToolsSection(): ReactNode {
  return (
    <section className={styles.tools} id="tools">
      <div className={styles.wrap}>
        <p className={styles.secTag} data-reveal>
          <b>//</b> 05 · mcp tools
        </p>
        <h2 className={styles.display} data-reveal>
          Twelve tools. One protocol.
        </h2>
        <p className={styles.sectionLead} data-reveal>
          Twelve read-only tools over Streamable HTTP (/mcp), with the legacy
          HTTP+SSE pair also served — declarative JSON schemas, cursor
          pagination, and full provenance on every answer.
        </p>
        <div className={styles.toolsGrid}>
          {MCP_TOOLS.map((tool, i) => (
            <Link
              key={tool.name}
              to={tool.href}
              className={`${styles.toolCard} ${['', styles.d1, styles.d2][i % 3] ?? ''}`}
              data-reveal>
              <span className={styles.toolTop}>
                <span className={styles.toolIdx}>{String(i + 1).padStart(2, '0')}</span>
                <span className={styles.toolCat}>{tool.cat}</span>
              </span>
              <span className={styles.toolName}>{tool.name}</span>
              <span className={styles.toolDesc}>{tool.desc}</span>
              <span className={styles.toolLink}>docs →</span>
            </Link>
          ))}
        </div>
      </div>
    </section>
  );
}
