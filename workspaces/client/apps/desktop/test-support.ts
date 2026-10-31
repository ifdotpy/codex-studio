import type { SavedRecovery } from "./recovery.ts";
import type { components } from "../web/src/generated/api.ts";
import { readFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
// Trusted VM replies and test-owned JSON documents retain their base behavior.
// This single indexed view supplies the fields that each witness inspects.
export function record(value: unknown): Record<string, unknown> {
  return value as Record<string, unknown>;
}
export function nodeFixture(name: string, interpreter = process.execPath) {
  return `#!${interpreter}\n${stripTypeScriptTypes(readFileSync(new URL(`./fixtures/${name}.mts`, import.meta.url), "utf8"))}`;
}
export function parsedRecord(source: string): Record<string, unknown> {
  return JSON.parse(source);
}

// Recovery fixtures read back the configuration emitted by configureRecovery.
export function parseRecoveryJSON(source: string): Omit<
  SavedRecovery,
  "environment" | "unsetEnvironment"
> & {
  environment: NodeJS.ProcessEnv;
  unsetEnvironment: string[];
} {
  return JSON.parse(source);
}
// plutil supplies this document; preserve the base's direct property reads.
export function parseLaunchAgentJSON(source: string): Record<
  string,
  components["schemas"]["JsonValue"]
> & {
  ProgramArguments: string[];
} {
  return JSON.parse(source);
}
