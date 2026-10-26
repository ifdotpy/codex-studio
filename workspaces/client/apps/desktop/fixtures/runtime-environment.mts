import * as http from "node:http";
import * as fs from "node:fs";
import * as path from "node:path";
import type { BackendIdentity } from "../backend.ts";
const state = process.env.CODEX_AGENTS_STATE_DIR!;
fs.writeFileSync(path.join(state, "fixture.pid"), String(process.pid));
http
  .createServer((_request, response) => {
    const identity = {
      application: "codex-agents",
      protocol: 1,
      backendKind: "rust",
      backendBuild: process.env.CODEX_BACKEND_BUILD!,
      pid: process.pid,
      stateDir: state,
      restartEnvironment: Object.fromEntries(
        ["CODEX_HOME", "CODEX_CANVAS_CWD"]
          .filter((key) => key in process.env)
          .map((key) => [key, process.env[key]!]),
      ),
    } satisfies BackendIdentity;
    response.end(JSON.stringify(identity));
  })
  .listen(Number(process.argv.at(-1)), "127.0.0.1");
