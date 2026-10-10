// Read MCP initialization metadata. Tool calls and model input are not sent.
import { createHash } from "node:crypto";
import { readExternalInstructions } from "./move-external.mjs";
let input = "";
for await (const chunk of process.stdin) {
  input += chunk;
  if (input.length > 1024 * 1024)
    throw new Error("MCP configuration exceeds its limit");
}
const { servers } = JSON.parse(input);
if (!Array.isArray(servers) || servers.length > 100)
  throw new Error("MCP server catalog exceeds its limit");
const proofs = await Promise.all(
  servers.map(async ({ name, config }) => {
    try {
      const instructions = await readExternalInstructions(config);
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
