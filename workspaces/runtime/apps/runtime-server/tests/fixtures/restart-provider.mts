#!/usr/bin/env node
// A scripted Codex app-server for the backend restart witness. It records
// every request in requests.jsonl, keeps turns running, holds command/exec
// until the test writes release-exec, and completes a turn of a thread when
// the test writes complete-<threadId>. It runs under the real supervisor, so
// it outlives the backend that started it.
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";

const root = path.dirname(fs.realpathSync(process.argv[1]));
const log = path.join(root, "requests.jsonl");
const pendingExec = new Map();
const runningTurns = new Map();
fs.appendFileSync(path.join(root, "provider-pids"), `${process.pid}\n`);

function send(value) {
  process.stdout.write(`${JSON.stringify(value)}\n`);
}
function notify(method, params) {
  send({ method, params });
}
function exists(name) {
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
  const request = JSON.parse(line);
  // The trailing newline publishes a complete request to the Rust witness.
  fs.appendFileSync(log, `${line}\n`);
  if (!Object.hasOwn(request, "id")) return;
  const method = request.method;
  const params = request.params ?? {};
  if (method === "command/exec") {
    pendingExec.set(request.id, params.processId);
    return;
  }
  let result = {};
  if (method === "thread/resume") result = { thread: { id: params.threadId } };
  else if (method === "turn/start") {
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
