// Read MCP initialization metadata. Tool calls and model input are not sent.
import { createHash } from "node:crypto";
import {
  readExternalInstructions,
  parseExternalConfig,
} from "./move-external.ts";
let input = "";
for await (const chunk of process.stdin) {
  input += chunk;
  if (input.length > 1024 * 1024)
    throw new Error("MCP configuration exceeds its limit");
}
const value: unknown = JSON.parse(input);
const servers: unknown =
  value !== null && typeof value === "object" && "servers" in value
    ? value.servers
    : undefined;
if (!Array.isArray(servers) || servers.length > 100)
  throw new Error("MCP server catalog exceeds its limit");
const proofs = await Promise.all(
  servers.map(async (server: unknown) => {
    if (
      server === null ||
      typeof server !== "object" ||
      !("name" in server) ||
      typeof server.name !== "string" ||
      !("config" in server)
    )
      throw new Error("Invalid MCP server declaration");
    const { name, config } = server;
    try {
      const instructions = await readExternalInstructions(
        parseExternalConfig(config),
      );
      if (instructions !== null && typeof instructions !== "string")
        throw new Error("Invalid MCP instructions");
      return {
        name,
        proofLevel: "initialize_instructions",
        instructionsHash: createHash("sha256")
          .update(JSON.stringify(instructions))
          .digest("hex"),
      };
    } catch {
      return { name, proofLevel: "server_info_fallback" };
    }
  }),
);
process.stdout.write(JSON.stringify(proofs));
