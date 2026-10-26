#!/usr/bin/env node
// A scripted Codex app-server for the backend restart witness. It records
// every request in requests.jsonl, keeps turns running, holds command/exec
// until the test writes release-exec, and completes a turn of a thread when
// the test writes complete-<threadId>. It runs under the real supervisor, so
// it outlives the backend that started it.
import fs from "node:fs";
import assert from "node:assert/strict";
import type { JsonValue } from "../../../../../client/apps/web/src/generated/api.ts";
import path from "node:path";
import readline from "node:readline";

const executable = process.argv[1];
assert.ok(executable);
const root = path.dirname(fs.realpathSync(executable));
const log = path.join(root, "requests.jsonl");
const pendingExec = new Map<JsonValue, JsonValue>();
const runningTurns = new Map<string, string>();
fs.appendFileSync(path.join(root, "provider-pids"), `${process.pid}\n`);

function send(value: JsonValue) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
function notify(method: string, params: JsonValue) {
  send({ method, params });
}
function exists(name: string) {
  return fs.existsSync(path.join(root, name));
}

setInterval(() => {
  if (exists("release-exec")) {
    for (const [id, processId] of pendingExec) {
      pendingExec.delete(id);
      fs.appendFileSync(path.join(root, "exec-released"), `${processId}\n`);
      send({ id, result: { exitCode: 3, stdout: "", stderr: "" } });
    }
  }
  for (const [threadId, turnId] of runningTurns) {
    if (!exists(`complete-${threadId}`)) continue;
    runningTurns.delete(threadId);
    const itemId = `answer-${turnId}`;
    notify("item/completed", {
      threadId,
      turnId,
      item: { id: itemId, type: "agentMessage", text: "fixture answer" },
    });
    notify("turn/completed", {
      threadId,
      turn: { id: turnId, status: "completed" },
    });
  }
}, 25);

readline.createInterface({ input: process.stdin }).on("line", (line) => {
  const request: { [key: string]: JsonValue } = JSON.parse(line);
  const params = request.params ?? {};
  assert.ok(params && typeof params === "object" && !Array.isArray(params));
  // The trailing newline publishes a complete request to the Rust witness.
  fs.appendFileSync(log, `${line}\n`);
  if (!Object.hasOwn(request, "id")) return;
  assert.ok(request.id !== undefined);
  const method = request.method;

  if (method === "command/exec") {
    assert.ok(params.processId !== undefined);
    pendingExec.set(request.id, params.processId);
    return;
  }
  let result = {};
  if (method === "thread/resume") result = { thread: { id: params.threadId } };
  else if (method === "turn/start") {
    assert.ok(typeof params.threadId === "string");
    const turnId = `turn-${params.threadId}`;
    runningTurns.set(params.threadId, turnId);
    result = { turn: { id: turnId } };
    send({ id: request.id, result });
    notify("turn/started", {
      threadId: params.threadId,
      turn: { id: turnId, status: "inProgress" },
    });
    return;
  } else if (method === "turn/interrupt") {
    assert.ok(typeof params.threadId === "string");
    assert.ok(params.turnId !== undefined);
    runningTurns.delete(params.threadId);
    send({ id: request.id, result: {} });
    notify("turn/completed", {
      threadId: params.threadId,
      turn: { id: params.turnId, status: "interrupted" },
    });
    return;
  } else if (method === "config/read") result = { config: {} };
  else if (method === "model/list")
    result = {
      data: [
        {
          id: "fake-model",
          model: "fake-model",
          displayName: "Fixture",
          supportedReasoningEfforts: [],
          defaultReasoningEffort: "medium",
        },
      ],
    };
  send({ id: request.id, result });
});
