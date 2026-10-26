import * as http from "node:http";
import * as fs from "node:fs";
import * as path from "node:path";
import type { BackendIdentity } from "../backend.ts";
const state = process.env.CODEX_AGENTS_STATE_DIR ?? "";
fs.writeFileSync(path.join(state, "fixture.pid"), String(process.pid));
http
  .createServer((_request, response) => {
    const identity = {
      application: "codex-agents",
      protocol: 1,
      backendKind: "rust",
      backendBuild: process.env.CODEX_BACKEND_BUILD ?? "",
      pid: process.pid,
      stateDir: state,
      restartEnvironment: Object.fromEntries(
        ["CODEX_HOME", "CODEX_CANVAS_CWD"].flatMap((key) => {
          const value = process.env[key];
          return value === undefined ? [] : [[key, value]];
        }),
      ),
    } satisfies BackendIdentity;
    response.end(JSON.stringify(identity));
  })
  .listen(Number(process.argv.at(-1)), "127.0.0.1");
