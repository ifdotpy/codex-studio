import * as http from "node:http";
import type { BackendIdentity } from "../backend.ts";
const port = Number(process.argv[process.argv.indexOf("--port") + 1]);
http
  .createServer((request, response) => {
    if (request.url !== "/api/desktop") {
      response.writeHead(404).end();
      return;
    }
    response.setHeader("content-type", "application/json");
    const identity = {
      application: "codex-agents",
      protocol: 1,
      backendKind: "rust",
      backendBuild: process.env.CODEX_BACKEND_BUILD ?? "",
      pid: process.pid,
      stateDir: process.env.CODEX_AGENTS_STATE_DIR ?? "",
    } satisfies BackendIdentity;
    response.end(JSON.stringify(identity));
  })
  .listen(port, "127.0.0.1");
const fixtureLifetimeMs = 15000;
setTimeout(() => process.exit(0), fixtureLifetimeMs);
