#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import { createServer } from "node:http";
import { delimiter, join } from "node:path";

export type BackendStartupMode = "exit" | "signal" | "path" | "http";

switch (process.env.FIXTURE_BACKEND_MODE) {
  case "exit":
    process.exit(7);
    break;
  case "signal":
    process.kill(process.pid, "SIGTERM");
    break;
  case "path": {
    const hidden = (process.env.PATH ?? "")
      .split(delimiter)
      .every((entry) => !existsSync(join(entry, "codex")));
    const node = spawnSync("/usr/bin/env", ["node", "--version"]);
    process.exit(hidden && node.status === 0 ? 0 : 9);
    break;
  }
  case "http": {
    assert.deepEqual(process.argv.slice(2), ["--port", "0"]);
    const server = createServer((_request, response) => {
      response.setHeader("Content-Type", "application/json");
      response.end(JSON.stringify({ token: "fixture-token" }));
    });
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      assert.ok(address && typeof address !== "string");
      console.log(`Codex Canvas: http://127.0.0.1:${address.port}`);
    });
    break;
  }
  default:
    throw new Error("Unknown backend startup fixture mode");
}
