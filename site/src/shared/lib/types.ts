import type {ReactNode} from 'react';

export type Tone = 'cmd' | 'ok' | 'dim' | 'hl';

export interface TermLine {
  readonly text: string;
  readonly tone: Tone;
}

export interface PipelineStage {
  readonly num: string;
  readonly title: string;
  readonly desc: string;
  readonly meta: string;
  readonly chips: readonly string[];
  readonly final?: boolean;
}

export interface Feature {
  readonly num: string;
  readonly icon: ReactNode;
  readonly name: string;
  readonly desc: string;
  readonly tags: readonly string[];
}

export interface FlowStep {
  readonly title: string;
  readonly desc: string;
}

export interface McpTool {
  readonly name: string;
  readonly cat: string;
  readonly href: string;
  readonly desc: string;
}

export type QuiStep = {cmd: string; title: string; desc: string};

export type UseCase = {tag: string; title: string; desc: string; chips: readonly string[]};

export type TechRow = {name: string; role: string; why: string};
