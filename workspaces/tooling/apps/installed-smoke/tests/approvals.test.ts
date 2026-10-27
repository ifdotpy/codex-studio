import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";

for (const scenario of [
  "success",
  "duplicate",
  "wrong-owner",
  "no-removal",
] as const) {
  test(`approval probe ${scenario}`, async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "approval-probe-"));
    const accepted = path.join(dir, "accepted");
    const declined = path.join(dir, "declined");
    let changed = false;
    let stream: http.ServerResponse | undefined;
    const document = (
      collection: string,
      id: string,
      seq: number,
      value?: object,
    ) =>
      value
        ? { collection, id, seq, op: "upsert", value }
        : { collection, id, seq, op: "delete" };
    const pending = [
      document("request", "r1", 1, {
        id: "r1",
        agent: scenario === "wrong-owner" ? "other" : "agent",
        method: "monitor/approve",
        status: "pending",
        params: { command: "echo run", cwd: "/tmp" },
      }),
      document("request", "r2", 2, {
        id: "r2",
        agent: "agent",
        method: "monitor/approve",
        status: "pending",
        params: { command: "echo run", cwd: "/tmp" },
      }),
      document("monitor", "m1", 3, {
        id: "m1",
        agent: "agent",
        status: "approval",
      }),
      document("monitor", "m2", 4, {
        id: "m2",
        agent: "agent",
        status: "approval",
      }),
    ];
    const resolved = [
      document("request", "r1", 5),
      document("request", "r2", 6),
      document("monitor", "m1", 7, {
        id: "m1",
        agent: "agent",
        status: "completed",
        exitCode: 0,
      }),
      document("monitor", "m2", 8, {
        id: "m2",
        agent: "agent",
        status: "cancelled",
      }),
    ];
    const server = http.createServer((req, res) => {
      const url = new URL(req.url ?? "", "http://fixture");
      if (url.pathname === "/api/sync/protocol")
        return res.end(JSON.stringify({ protocolVersion: 4 }));
      if (url.pathname === "/api/sync/stream") {
        stream = res;
        res.writeHead(200, { "content-type": "text/event-stream" });
        res.write(
          `event: resources\ndata: ${JSON.stringify({ reason: "initial", resourceVersions: [] })}\n\n`,
        );
        return;
      }
      const after = Number(url.searchParams.get("after"));
      const documents = [...pending, ...(changed ? resolved : [])].filter(
        (item) => item.seq > after,
      );
      return res.end(
        JSON.stringify({
          checkpoint: { seq: documents.at(-1)?.seq ?? after },
          maxSeq: changed ? 8 : 4,
          documents,
        }),
      );
    });
    await new Promise<void>((resolve) =>
      server.listen(0, "127.0.0.1", resolve),
    );
    const address = server.address();
    assert.ok(address && typeof address !== "string");
    const child = spawn(process.execPath, [
      "workspaces/tooling/apps/installed-smoke/approvals.ts",
      `http://127.0.0.1:${address.port}`,
      "agent",
      "m1",
      "r1",
      accepted,
      "m2",
      "r2",
      declined,
      "1",
      path.join(dir, "evidence.json"),
    ]);
    let output = "";
    child.stdout.on("data", (chunk: Buffer) => {
      output += chunk.toString();
      if (output.includes("READY") && !changed) {
        changed = true;
        fs.writeFileSync(
          accepted,
          scenario === "duplicate" ? "run\nrun\n" : "run\n",
        );
        const documents =
          scenario === "no-removal"
            ? resolved.filter((doc) => doc.id !== "r2")
            : resolved;
        stream?.write(
          `event: resources\ndata: ${JSON.stringify({ reason: "change", resourceVersions: [{ entityChanges: { documents } }] })}\n\n`,
        );
      }
    });
    child.stderr.on("data", (chunk: Buffer) => {
      output += chunk.toString();
    });
    try {
      const code = await new Promise<number | null>((resolve) =>
        child.on("exit", resolve),
      );
      assert.equal(code, scenario === "success" ? 0 : 1, output);
      assert.ok(fs.existsSync(path.join(dir, "evidence.json")));
      assert.match(
        output,
        scenario === "success"
          ? /PASS: both cards resolved/
          : scenario === "duplicate"
            ? /marker has 2/
            : scenario === "wrong-owner"
              ? /Did not observe both/
              : /removal was not witnessed/,
      );
    } finally {
      stream?.destroy();
      server.close();
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
}
