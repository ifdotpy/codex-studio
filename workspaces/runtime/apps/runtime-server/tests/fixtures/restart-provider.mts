#!/usr/bin/env node
// A scripted Codex app-server for the backend restart witness. It records
// every request in requests.jsonl, keeps turns running, holds command/exec
// until the test writes release-exec, and completes a turn of a thread when
// the test writes complete-<threadId>. It runs under the real supervisor, so
// it outlives the backend that started it.
import fs from "node:fs";
import type { JsonValue } from "../../../../../client/apps/web/src/generated/api.ts";
import path from "node:path";
import readline from "node:readline";

// Requests come from the parent Rust witness; retain its JSON payload unchanged.
type FixtureRequest = Record<string, JsonValue> & { id?: JsonValue } & (
    | { method: "command/exec"; params: { processId: string } }
    | {
        method: "thread/resume" | "turn/start";
        params: { threadId: string };
      }
    | {
        method: "turn/interrupt";
        params: { threadId: string; turnId: string };
      }
    | {
        method:
          | "initialize"
          | "initialized"
          | "config/read"
          | "model/list"
          | "thread/backgroundTerminals/clean";
        params?: Record<string, JsonValue>;
      }
  );
function parseFixtureRequest(line: string) {
  return JSON.parse(line) as FixtureRequest;
}
const root = import.meta.dirname;
const log = path.join(root, "requests.jsonl");
const pendingExec = new Map<JsonValue, JsonValue>();
const runningTurns = new Map<JsonValue, JsonValue>();
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
  const request = parseFixtureRequest(line);
  // The trailing newline publishes a complete request to the Rust witness.
  fs.appendFileSync(log, `${line}\n`);
  if (!("id" in request)) return;
  const method = request.method;

  if (method === "command/exec") {
    pendingExec.set(request.id, request.params.processId);
    return;
  }
  let result = {};
  if (method === "thread/resume")
    result = { thread: { id: request.params.threadId } };
  else if (method === "turn/start") {
    const { threadId } = request.params;
    const turnId = `turn-${threadId}`;
    runningTurns.set(threadId, turnId);
    result = { turn: { id: turnId } };
    send({ id: request.id, result });
    notify("turn/started", {
      threadId,
      turn: { id: turnId, status: "inProgress" },
    });
    return;
  } else if (method === "turn/interrupt") {
    const { threadId, turnId } = request.params;
    runningTurns.delete(threadId);
    send({ id: request.id, result: {} });
    notify("turn/completed", {
      threadId,
      turn: { id: turnId, status: "interrupted" },
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
