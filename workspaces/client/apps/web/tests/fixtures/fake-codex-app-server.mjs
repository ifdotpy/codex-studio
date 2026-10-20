#!/usr/bin/env node
// Scripted Codex app-server for browser fixtures: JSON-RPC lines on stdin and
// stdout. It answers every request, serves a fixed model catalog, and completes
// each turn with one short assistant message. No network, no model call.
import { createInterface } from "node:readline";
import { readFileSync } from "node:fs";

if (process.argv.includes("--version")) {
  process.stdout.write("codex-cli 99.0.0\n");
  process.exit(0);
}

// FAKE_CODEX_MODELS names a JSON file holding the model/list "data" array.
const modelsFile = process.env.FAKE_CODEX_MODELS;
const models = modelsFile
  ? JSON.parse(readFileSync(modelsFile, "utf8"))
  : ["gpt-5.6-sol", "gpt-5.6-luna", "gpt-6-luna", "gpt-6-astra"].map(
      (model) => ({
        id: model,
        model,
        displayName: model,
        isDefault: model === "gpt-5.6-sol",
        defaultReasoningEffort: "low",
        supportedReasoningEfforts: ["low", "medium", "high"].map(
          (reasoningEffort) => ({ reasoningEffort }),
        ),
      }),
    );
// Turns whose input contains this text never complete.
const holdPrompt = process.env.FAKE_CODEX_HOLD_PROMPT;
let turnCount = 0;

const send = (message) => process.stdout.write(JSON.stringify(message) + "\n");

function startTurn(threadId) {
  const turnId = `fixture-turn-${++turnCount}`;
  send({
    method: "turn/started",
    params: { threadId, turn: { id: turnId, status: "inProgress" } },
  });
  return turnId;
}

function completeTurn(threadId) {
  const turnId = startTurn(threadId);
  const itemId = `fixture-item-${turnCount}`;
  const text = "Fixture reply";
  send({
    method: "item/started",
    params: {
      threadId,
      turnId,
      item: { id: itemId, type: "agentMessage", text: "" },
    },
  });
  send({
    method: "item/agentMessage/delta",
    params: { threadId, turnId, itemId, delta: text },
  });
  send({
    method: "item/completed",
    params: {
      threadId,
      turnId,
      item: { id: itemId, type: "agentMessage", text },
    },
  });
  send({
    method: "turn/completed",
    params: { threadId, turn: { id: turnId, status: "completed" } },
  });
}

function result(method, params) {
  switch (method) {
    case "thread/start":
      return { thread: { id: `fixture-thread-${Date.now()}-${turnCount}` } };
    case "thread/resume":
      return { thread: { id: params.threadId } };
    case "model/list":
      return { data: models };
    default:
      return {};
  }
}

createInterface({ input: process.stdin }).on("line", (line) => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  const { method, params = {} } = request;
  if (method === "turn/start") {
    const turnId = `fixture-turn-${turnCount + 1}`;
    send({ id: request.id, result: { turn: { id: turnId } } });
    // A held turn stays in flight, as a long-running model turn would.
    if (holdPrompt && JSON.stringify(params.input).includes(holdPrompt))
      startTurn(params.threadId);
    else setImmediate(() => completeTurn(params.threadId));
    return;
  }
  send({ id: request.id, result: result(method, params) });
});
