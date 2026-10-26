import fs from "node:fs";
import { createInterface } from "node:readline";
import type {
  Tool,
  JSONRPCRequest,
  InitializeRequest,
} from "@modelcontextprotocol/sdk/types.js";

const [, , log, mode, toolJson] = process.argv;

// The test writes these SDK-owned tool definitions as JSON in the same run.
function fixtureTools(text: string): Tool[] {
  return JSON.parse(text);
}
const tools = mode === "instructions" ? undefined : fixtureTools(toolJson!);
// The MCP client writes these requests using its own wire contract.
function fixtureRequest(
  line: string,
): JSONRPCRequest & { params?: InitializeRequest["params"] } {
  return JSON.parse(line);
}
for await (const line of createInterface({ input: process.stdin })) {
  const request = fixtureRequest(line);
  fs.appendFileSync(log!, request.method + "\n");
  if (!("id" in request)) continue;
  const result =
    request.method === "initialize" || mode === "instructions"
      ? {
          protocolVersion: request.params!.protocolVersion,
          capabilities: { tools: {} },
          serverInfo: { name: "fixture", version: "1" },
          ...(mode === "instructions"
            ? { instructions: "Private instructions" }
            : {}),
        }
      : request.method === "tools/list"
        ? { tools }
        : {};
  process.stdout.write(
    JSON.stringify({ jsonrpc: "2.0", id: request.id, result }) + "\n",
  );
}
