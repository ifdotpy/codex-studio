import fs from "node:fs";
import { createInterface } from "node:readline";
import type { Tool } from "@modelcontextprotocol/sdk/types.js";
import { isRecord } from "./move.ts";

const [, , log, mode, toolJson] = process.argv;
if (!log) throw new Error("Missing fixture log path");

// The test writes these SDK-owned tool definitions as JSON in the same run.
function fixtureTools(text: string): Tool[] {
  return JSON.parse(text);
}
const tools = fixtureTools(toolJson ?? "[]");
for await (const line of createInterface({ input: process.stdin })) {
  const request: unknown = JSON.parse(line);
  if (!isRecord(request)) throw new Error("Invalid fixture request");
  fs.appendFileSync(log, String(request.method) + "\n");
  if (!("id" in request)) continue;
  const params = isRecord(request.params) ? request.params : {};
  const initialized = {
    protocolVersion: params.protocolVersion,
    capabilities: { tools: {} },
    serverInfo: { name: "fixture", version: "1" },
    ...(mode === "instructions"
      ? { instructions: "Private instructions" }
      : {}),
  };
  const result =
    request.method === "initialize" || mode === "instructions"
      ? initialized
      : request.method === "tools/list"
        ? { tools }
        : {};
  process.stdout.write(
    JSON.stringify({ jsonrpc: "2.0", id: request.id, result }) + "\n",
  );
}
