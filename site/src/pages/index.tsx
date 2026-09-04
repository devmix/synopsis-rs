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
      title="Structured information for AI agents via MCP"
      description="synopsis[memex] is a zero-infrastructure knowledge base: hybrid search and an in-memory knowledge graph in one Go binary, exposed as 12 MCP tools.">
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
