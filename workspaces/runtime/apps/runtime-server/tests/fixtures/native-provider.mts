#!/usr/bin/env node
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import readline from "node:readline";

const root = path.dirname(fs.realpathSync(process.argv[1]));
let turnCount = 0;
fs.writeFileSync(path.join(root, "startup-cwd"), process.cwd());
async function reportSocket(socketPath, value) {
  await new Promise((resolve, reject) => {
    const socket = net.createConnection(socketPath);
    socket.once("error", reject);
    socket.once("connect", () => socket.end(`${JSON.stringify(value)}\n`));
    // Flush our report without waiting for the observer to finish its test.
    socket.once("finish", () => {
      socket.destroy();
      resolve();
    });
  });
}
async function report(name, value) {
  const socketPath = path.join(root, `${name}.sock`);
  if (fs.existsSync(socketPath)) await reportSocket(socketPath, value);
}
function increment(name) {
  const file = path.join(root, name);
  const count = fs.existsSync(file)
    ? Number(fs.readFileSync(file, "utf8")) + 1
    : 1;
  fs.writeFileSync(file, String(count));
  return count;
}
function notify(method, params) {
  console.log(JSON.stringify({ method, params }));
}
if (process.env.FIXTURE_CWD_SOCKET) {
  await reportSocket(process.env.FIXTURE_CWD_SOCKET, {
    pid: process.pid,
    cwd: process.cwd(),
  });
}
for await (const line of readline.createInterface({ input: process.stdin })) {
  const request = JSON.parse(line);
  const method = request.method;
  const accountProvider =
    request.params?.clientInfo?.name === "codex_agents_canvas";
  if (method === "initialize" && accountProvider) {
    const count = increment("account-launch-count");
    await report("launch", { pid: process.pid, count });
    if (count === 1 && fs.existsSync(path.join(root, "crash-first-initialize")))
      process.exit(17);
  }
  if (!Object.hasOwn(request, "id")) continue;
  let result = {};
  if (method === "thread/resume")
    result = { thread: { id: request.params.threadId } };
  else if (method === "turn/start") {
    turnCount = increment("turn-count");
    await report("turn", {
      pid: process.pid,
      cwd: process.cwd(),
      count: turnCount,
      priority: os.getPriority(),
      request,
    });
    if (turnCount === 1 && fs.existsSync(path.join(root, "hang-first-turn"))) {
      await new Promise(() => {});
    }
    result = { turn: { id: "fixture-turn" } };
  } else if (method === "fixture/barrier") {
    await reportSocket(path.join(root, "turn.sock"), {
      method,
      count: turnCount,
    });
    result = { count: turnCount };
  } else if (method === "model/list") {
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
  }
  console.log(JSON.stringify({ id: request.id, result }));
  if (
    method === "initialize" &&
    accountProvider &&
    fs.existsSync(path.join(root, "emit-generation-notification"))
  ) {
    notify("fixture/generation", { pid: process.pid });
  }
  if (
    method === "turn/start" &&
    process.env.STUDIO_W7_P01_COMPLETE_TURN === "1"
  ) {
    const threadId = request.params.threadId,
      turnId = "fixture-turn",
      itemId = "fixture-assistant-item";
    notify("turn/started", {
      threadId,
      turn: { id: turnId, status: "inProgress" },
    });
    notify("item/started", {
      threadId,
      turnId,
      item: { id: itemId, type: "agentMessage", text: "" },
    });
    notify("item/agentMessage/delta", {
      threadId,
      turnId,
      itemId,
      delta: "fixture assistant response",
    });
    notify("item/completed", {
      threadId,
      turnId,
      item: {
        id: itemId,
        type: "agentMessage",
        text: "fixture assistant response",
      },
    });
    notify("turn/completed", {
      threadId,
      turn: { id: turnId, status: "completed" },
    });
  }
}
