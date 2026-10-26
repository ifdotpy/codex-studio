import type { InlineConfig, Plugin, ResolveOptions } from "vite";

export interface NativeGraphException {
  path: string;
  reason: string;
}
export interface NativeGraphOptions {
  entry: string;
  uiAdapters?: NativeGraphException[];
  readonlyMigrations?: NativeGraphException[];
}
export interface NativeGraphFinding {
  module: string;
  capability: string;
  detail: string;
  line?: number;
  column?: number;
  trace: string[];
}
export interface NativeArtifact {
  path: string;
  bytes: number;
  sha256: string;
}
export interface NativeSourceInventory {
  id: string;
  sourceHash: string;
  transformedHash: string;
  disposition: "omitted" | "retained";
  renderedBytes: number;
  emissions: {
    artifact: string;
    renderedHash: string;
    renderedBytes: number;
  }[];
}
export interface NativeArtifactAudit {
  version: number;
  auditorHash: string;
  entry: string;
  contract: {
    entry: string;
    uiAdapters: NativeGraphException[];
    readonlyMigrations: NativeGraphException[];
  };
  sourceInventoryHash: string;
  sources: NativeSourceInventory[];
  artifacts: NativeArtifact[];
  hash: string;
}
export interface NativeGraphReport {
  entry: string;
  modules: string[];
  findings: NativeGraphFinding[];
  accepted: boolean;
  inventory?: { modules: NativeSourceInventory[]; artifacts: NativeArtifact[] };
  receipt?: NativeArtifactAudit;
}
export interface NativeGraphChecker {
  plugin: Plugin;
  report(): NativeGraphReport | undefined;
  finalize(output: unknown): Promise<NativeGraphAcceptedReport>;
}
export type NativeGraphAcceptedReport = NativeGraphReport & {
  accepted: true;
  receipt: NativeArtifactAudit;
};
export const NATIVE_AUDIT_FILENAME: string;
export function inspectCapabilities(
  code: string,
  id: string,
  permissions: { ui: boolean; readonly: boolean },
): Omit<NativeGraphFinding, "module" | "trace">[];
export function nativeImportGraphPlugin(
  options: NativeGraphOptions,
): NativeGraphChecker;
export function finalizeNativeImportGraph(
  checker: NativeGraphChecker,
  output: unknown,
): Promise<NativeGraphAcceptedReport>;
export function verifyNativeArtifactAudit(options: {
  outDir: string;
  expected?: NativeGraphOptions & { root: string };
}): Promise<NativeArtifactAudit>;
export function buildNativeRenderer(
  config: InlineConfig,
  options: NativeGraphOptions,
): Promise<{
  output: unknown;
  report: NativeGraphAcceptedReport;
  receipt: NativeArtifactAudit;
}>;
export function checkNativeImportGraph(
  options: NativeGraphOptions & { root: string; resolve?: ResolveOptions },
): Promise<NativeGraphAcceptedReport>;
