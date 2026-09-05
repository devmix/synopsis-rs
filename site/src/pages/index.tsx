import Layout from '@theme/Layout';
import type {ReactNode} from 'react';

import styles from './index.module.css';

import {useRevealOnScroll} from '../shared/lib/hooks/useRevealOnScroll';
import {ArchitectureSection} from '../widgets/architecture';
import {CtaSection} from '../widgets/cta';
import {FeaturesSection} from '../widgets/features';
import {Hero} from '../widgets/hero';
import {HowSection} from '../widgets/how';
import {PipelineSection} from '../widgets/pipeline';
import {QuickstartSection} from '../widgets/quickstart';
import {TechSection} from '../widgets/tech';
import {Ticker} from '../widgets/ticker';
import {ToolsSection} from '../widgets/tools';
import {UseCasesSection} from '../widgets/usecases';

/* ============================= page ============================= */

export default function Home(): ReactNode {
  useRevealOnScroll();

  return (
    <Layout
      title="Local RAG + knowledge-graph MCP server in Rust"
      description="synopsis[memex] is a local RAG + knowledge-graph MCP server in Rust: one binary, hybrid search and a knowledge graph, exposed as 12 read-only MCP tools. No external services.">
      <div className={styles.page}>
        <div className={styles.noise} aria-hidden="true" />
        <noscript>
          <style>{'[data-reveal]{opacity:1 !important;transform:none !important}'}</style>
        </noscript>
        <main>
          <Hero />
          <Ticker />
          <ArchitectureSection />
          <PipelineSection />
          <FeaturesSection />
          <HowSection />
          <QuickstartSection />
          <ToolsSection />
          <UseCasesSection />
          <TechSection />
          <CtaSection />
        </main>
      </div>
    </Layout>
  );
}
