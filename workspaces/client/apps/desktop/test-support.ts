import { readFileSync } from "node:fs";
import { stripTypeScriptTypes } from "node:module";
import assert from "node:assert/strict";
import type { Server } from "node:net";
export function present<T>(value: T | null | undefined): T {
  assert(value !== undefined && value !== null);
  return value;
}
export function portOf(server: Pick<Server, "address">): number {
  const address = server.address();
  assert(address && typeof address !== "string");
  return address.port;
}
// VM and JSON fixtures cross an untyped boundary; narrow fields before reading.
export function record(value: unknown): Record<string, unknown> {
  assert(value && typeof value === "object");
  return value as Record<string, unknown>;
}
export type RawNativeAction = (
  event: unknown,
  request: unknown,
) => Promise<unknown>;

export function nodeFixture(name: string) {
  return `#!${process.execPath}\n${stripTypeScriptTypes(readFileSync(new URL(`./fixtures/${name}.mts`, import.meta.url), "utf8"))}`;
}
