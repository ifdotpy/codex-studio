import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import type { components } from "../../../../client/apps/web/src/generated/api.ts";

type Schema = components["schemas"];
// Safety bound for a broken fixture; normal completion follows the SSE event.
const fixtureDeadlineSeconds = 60;

/** A pending monitor start, as the server publishes it in the request entity. */
function pendingStart(id: string, agent: string): Schema["Approval"] {
  return {
    id,
    agent,
    turn: null,
    created: 1,
    epoch: null,
    request: { kind: "monitor_start", monitor: "m", command: "echo run" },
    delivery: null,
    detail: {
      params: {},
      preview: null,
      answer_error: null,
      host_close_reason: null,
    },
    state: { kind: "pending" },
    deferral: null,
    fire_at: null,
    hidden: null,
  };
}

for (const scenario of [
  "success",
  "duplicate",
  "wrong-owner",
  "no-removal",
  "invalid-request",
  "invalid-monitor",
] as const) {
  test(`approval probe ${scenario}`, async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), "approval-probe-"));
    const accepted = path.join(dir, "accepted");
    const declined = path.join(dir, "declined");
    let changed = false;
    let stream: http.ServerResponse | undefined;
    const pending = [
      {
        collection: "request",
        id: "r1",
        seq: 1,
        op: "upsert",
        value: pendingStart(
          "r1",
          scenario === "wrong-owner" ? "other" : "agent",
        ),
      },
      {
        collection: "request",
        id: "r2",
        seq: 2,
        op: "upsert",
        value: pendingStart("r2", "agent"),
      },
      {
        collection: "monitor",
        id: "m1",
        seq: 3,
        op: "upsert",
        value: { id: "m1", agent: "agent", status: "approval" },
      },
      {
        collection: "monitor",
        id: "m2",
        seq: 4,
        op: "upsert",
        value: { id: "m2", agent: "agent", status: "approval" },
      },
    ] satisfies Schema["SyncEntity"][];
    const resolved = [
      { collection: "request", id: "r1", seq: 5, op: "delete" },
      { collection: "request", id: "r2", seq: 6, op: "delete" },
      {
        collection: "monitor",
        id: "m1",
        seq: 7,
        op: "upsert",
        value: { id: "m1", agent: "agent", status: "completed", exitCode: 0 },
      },
      {
        collection: "monitor",
        id: "m2",
        seq: 8,
        op: "upsert",
        value: { id: "m2", agent: "agent", status: "cancelled" },
      },
    ] satisfies Schema["SyncEntity"][];
    const event = (
      reason: "initial" | "change",
      documents: Schema["SyncEntity"][],
    ): Schema["ResourceChangeEvent"] => ({
      epoch: "epoch",
      protocol: 4,
      reason,
      resources: [{ kind: "state" }],
      resourceVersions: [
        {
          resource: { kind: "state" },
          revision: 8,
          entityChanges: { after: 4, through: 8, documents },
        },
      ],
      revision: 8,
      workspaceId: "fixture",
    });
    const server = http.createServer((req, res) => {
      const url = new URL(req.url ?? "", "http://fixture");
      if (url.pathname === "/api/sync/protocol")
        return res.end(
          JSON.stringify({
            protocolVersion: 4,
            capabilities: [],
            maxEntityPage: 500,
            maxOtherPage: 100,
            maxStreamBytes: 1,
            maxStreamDocuments: 1,
            pullEndpoint: "/api/sync/pull",
            scopes: [],
            streamEndpoint: "/api/sync/stream",
            supportedVersions: [4],
          } satisfies Schema["SyncProtocolResponse"]),
        );
      if (url.pathname === "/api/sync/stream") {
        if (
          url.searchParams.get("since") !==
          JSON.stringify([
            {
              resource: { kind: "state" },
              since: { seq: 2, workspaceId: "fixture" },
            },
          ])
        ) {
          res.writeHead(400);
          return res.end("Resume from the baseline to avoid a full transfer");
        }
        stream = res;
        res.writeHead(200, { "content-type": "text/event-stream" });
        let initial = JSON.stringify(
          event(
            "initial",
            pending.filter((doc) => doc.collection === "monitor"),
          ),
        );
        if (scenario === "invalid-monitor")
          initial = initial.replace('"status":"approval"', '"status":"bogus"');
        res.write(`event: resources\ndata: ${initial}\n\n`);
        return;
      }
      const after = Number(url.searchParams.get("after"));
      const documents = [...pending, ...(changed ? resolved : [])].filter(
        (item) => item.collection === "request" && item.seq > after,
      );
      let page = JSON.stringify({
        generation: 1,
        workspaceId: "fixture",
        checkpoint: { seq: documents.at(-1)?.seq ?? after },
        maxSeq: changed ? 6 : 2,
        documents,
      } satisfies Schema["SyncPullResponse"]);
      if (scenario === "invalid-request")
        page = page.replace(
          '"state":{"kind":"pending"}',
          '"state":{"kind":"bogus"}',
        );
      return res.end(page);
    });
    await new Promise<void>((resolve) =>
      server.listen(0, "127.0.0.1", resolve),
    );
    const address = server.address();
    assert.ok(address && typeof address !== "string");
    const child = spawn(
      process.execPath,
      [
        "--import",
        "./workspaces/tooling/apps/installed-smoke/tests/fixtures/approval-deadline.ts",
        "workspaces/tooling/apps/installed-smoke/approvals.ts",
        `http://127.0.0.1:${address.port}`,
        "agent",
        "m1",
        "r1",
        accepted,
        "m2",
        "r2",
        declined,
        String(fixtureDeadlineSeconds),
        path.join(dir, "evidence.json"),
      ],
      {
        env: {
          ...process.env,
          APPROVAL_DEADLINE_EVENT:
            scenario === "wrong-owner" ? "initial" : "change",
        },
      },
    );
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
        assert.ok(stream);
        stream.write(
          `event: resources\ndata: ${JSON.stringify(event("change", documents))}\n\n`,
        );
      }
    });
    child.stderr.on("data", (chunk: Buffer) => {
      output += chunk.toString();
    });
    try {
      const code = await new Promise<number | null>((resolve) =>
        child.on("close", resolve),
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
              : scenario === "invalid-request"
                ? /Invalid request/
                : scenario === "invalid-monitor"
                  ? /Invalid monitor/
                  : /decline: approval request removal was not witnessed/,
      );
    } finally {
      stream?.destroy();
      server.close();
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
}
