#!/usr/bin/env node
// Scripted Codex app-server for browser fixtures: JSON-RPC lines on stdin and
// stdout. It answers every request, serves a fixed model catalog, and completes
// each turn with one short assistant message unless a script says otherwise.
// No network, no model call.
//
// Environment:
//   FAKE_CODEX_MODELS   JSON file holding the model/list "data" array
//   FAKE_CODEX_SCRIPT   JSON file: [{"prompt": text, "steps": [step...]}]. A
//                       turn whose input contains `prompt` runs `steps`
//                       instead of the default reply.
//   FAKE_CODEX_CONTROL  directory. Each provider process listens on
//                       <directory>/<pid>.sock. A client sends a JSON line
//                       {"id", "thread", "steps"}; the process that owns the
//                       thread runs `steps` on its turn in flight and answers
//                       {"id","ok":true}; others answer {"id","ok":false}.
//
// Steps run in order:
//   {"say": text, "extra": {}}      assistant message, streamed as one delta;
//                                   extra fields join the completed item
//   {"delta": text, "item": id}     start a streaming assistant message
//   {"delta": text, "item": id, "append": true}   extend that message
//   {"tool": name, "arguments": {}} dynamic tool call; waits for the reply
//   {"notify": {method, params}}    server notification; threadId and turnId
//                                   are added to params
//   {"request": {method, params}}   server request; waits for the reply
//   {"fail": text}                  end the turn as failed with this message
//   {"hold": true}                  keep the turn in flight (script stops)
//   {"complete": true}              finish the turn (ends a hold)
// A script that does not hold completes its turn after the last step. A held
// turn also completes, as interrupted, on turn/interrupt.
import { createInterface } from "node:readline";
import { createServer } from "node:net";
import { readFileSync, rmSync } from "node:fs";
import { join } from "node:path";

if (process.argv.includes("--version")) {
  process.stdout.write("codex-cli 99.0.0\n");
  process.exit(0);
}

const models = process.env.FAKE_CODEX_MODELS
  ? JSON.parse(readFileSync(process.env.FAKE_CODEX_MODELS, "utf8"))
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
const scripts = process.env.FAKE_CODEX_SCRIPT
  ? JSON.parse(readFileSync(process.env.FAKE_CODEX_SCRIPT, "utf8"))
  : [];
const DEFAULT_STEPS = [{ say: "Fixture reply" }];

let turnCount = 0;
let threadCount = 0;
let callCount = 0;
/** Turn in flight per thread: thread id -> turn id. */
const turns = new Map();
/** Waiters for replies to requests this provider sent: request id -> resolve. */
const replies = new Map();
/** Ends of held turns: turn id -> {complete, interrupt}. */
const holds = new Map();

const send = (message) => process.stdout.write(JSON.stringify(message) + "\n");
const notify = (method, params) => send({ method, params });

function call(method, params) {
  const id = `fixture-call-${++callCount}`;
  return new Promise((resolve) => {
    replies.set(id, resolve);
    send({ id, method, params });
  });
}

async function runStep(threadId, turnId, step) {
  const itemId = step.item ?? `${turnId}-item-${++callCount}`;
  if ("say" in step) {
    notify("item/started", {
      threadId,
      turnId,
      item: { id: itemId, type: "agentMessage", text: "" },
    });
    notify("item/agentMessage/delta", {
      threadId,
      turnId,
      itemId,
      delta: step.say,
    });
    notify("item/completed", {
      threadId,
      turnId,
      item: {
        id: itemId,
        type: "agentMessage",
        text: step.say,
        ...step.extra,
      },
    });
  } else if ("delta" in step) {
    if (!step.append)
      notify("item/started", {
        threadId,
        turnId,
        item: { id: itemId, type: "agentMessage", text: "" },
      });
    notify("item/agentMessage/delta", {
      threadId,
      turnId,
      itemId,
      delta: step.delta,
    });
  } else if ("tool" in step) {
    await call("item/tool/call", {
      threadId,
      turnId,
      callId: `fixture-tool-${++callCount}`,
      tool: step.tool,
      arguments: step.arguments ?? {},
    });
  } else if ("notify" in step) {
    notify(step.notify.method, { threadId, turnId, ...step.notify.params });
  } else if ("request" in step) {
    await call(step.request.method, {
      threadId,
      turnId,
      ...step.request.params,
    });
  } else if (step.complete) {
    holds.get(turnId)?.complete();
  } else throw new Error(`Unknown fixture step ${JSON.stringify(step)}`);
}

/**
 * Run steps on a turn. Returns "hold" when a step kept the turn open, the
 * failure message when one ended it as failed, and null otherwise.
 */
async function runSteps(threadId, turnId, steps) {
  for (const step of steps) {
    if (step.hold) return "hold";
    if (step.fail !== undefined) return { failure: step.fail };
    await runStep(threadId, turnId, step);
  }
  return null;
}

async function runTurn(threadId, turnId, input) {
  notify("turn/started", {
    threadId,
    turn: { id: turnId, status: "inProgress" },
  });
  const text = JSON.stringify(input);
  const steps =
    scripts.find((script) => text.includes(script.prompt))?.steps ??
    DEFAULT_STEPS;
  let turn = { id: turnId, status: "completed" };
  const outcome = await runSteps(threadId, turnId, steps);
  if (outcome === "hold")
    turn.status = await new Promise((resolve) =>
      holds.set(turnId, {
        complete: () => resolve("completed"),
        interrupt: () => resolve("interrupted"),
      }),
    );
  else if (outcome)
    turn = {
      id: turnId,
      status: "failed",
      error: { message: outcome.failure },
    };
  turns.delete(threadId);
  holds.delete(turnId);
  notify("turn/completed", { threadId, turn });
}

function result(method, params) {
  switch (method) {
    case "thread/start":
      return { thread: { id: `fixture-thread-${++threadCount}` } };
    case "thread/resume":
      return { thread: { id: params.threadId } };
    case "model/list":
      return { data: models };
    default:
      return {};
  }
}

createInterface({ input: process.stdin }).on("line", (line) => {
  const message = JSON.parse(line);
  if (message.method === undefined) {
    replies.get(message.id)?.(message);
    replies.delete(message.id);
    return;
  }
  if (message.id === undefined) return;
  const { method, params = {} } = message;
  if (method === "turn/start") {
    // The id is fixed here: concurrent turns must report the id this reply names.
    const turnId = `fixture-turn-${++turnCount}`;
    turns.set(params.threadId, turnId);
    send({ id: message.id, result: { turn: { id: turnId } } });
    setImmediate(() => runTurn(params.threadId, turnId, params.input));
    return;
  }
  if (method === "turn/interrupt") {
    holds.get(params.turnId)?.interrupt();
    send({ id: message.id, result: {} });
    return;
  }
  send({ id: message.id, result: result(method, params) });
});

if (process.env.FAKE_CODEX_CONTROL) {
  const path = join(process.env.FAKE_CODEX_CONTROL, `${process.pid}.sock`);
  process.on("exit", () => rmSync(path, { force: true }));
  createServer((socket) => {
    createInterface({ input: socket }).on("line", async (line) => {
      const { id, thread, steps } = JSON.parse(line);
      const turnId = turns.get(thread);
      if (turnId) await runSteps(thread, turnId, steps);
      socket.write(JSON.stringify({ id, ok: turnId !== undefined }) + "\n");
    });
  }).listen(path);
}
