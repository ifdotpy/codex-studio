#!/usr/bin/env node
// Real HTTP and SQLite, deterministic Codex protocol fixture, no model inference.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { randomUUID } from "node:crypto";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(skill, "web/package.json"));
const { JSDOM } = require("jsdom");
const root = await mkdtemp(join(tmpdir(), "codex-product-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let errors = "",
  dom;
proc.stderr.on("data", (d) => (errors += d));
const wait = async (fn, label) => {
  const end = Date.now() + 10000;
  while (Date.now() < end) {
    if (fn()) return;
    await new Promise((r) => setTimeout(r, 20));
  }
  throw Error(label + " " + errors);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(Error(errors)));
  });
  const origin = `http://127.0.0.1:${port}`;
  dom = new JSDOM(await readFile(join(skill, "web/index.html"), "utf8"), {
    url: origin,
    runScripts: "outside-only",
    pretendToBeVisual: true,
  });
  const w = dom.window,
    $ = (s) => w.document.querySelector(s);
  w.ResizeObserver = class {
    observe() {}
  };
  w.HTMLElement.prototype.setPointerCapture = () => {};
  w.HTMLDialogElement.prototype.showModal = function () {
    this.open = true;
  };
  w.HTMLDialogElement.prototype.close = function () {
    this.open = false;
  };
  w.crypto.randomUUID = randomUUID;
  let loseCreate = true,
    loseSend = false;
  w.fetch = async (url, options) => {
    const response = await fetch(new URL(url, origin), options);
    if (url === "/api/leads" && loseCreate) {
      loseCreate = false;
      throw Error("Lost creation reply");
    }
    if (url === "/api/messages" && options?.method === "POST" && loseSend) {
      loseSend = false;
      throw Error("Lost message reply");
    }
    return response;
  };
  const initial = await (await fetch(origin + "/api/state")).json();
  const lead = initial.runtime.agents.find((a) => a.name === "Release lead");
  const layout = {
    positions: { [lead.id]: { x: 517, y: 319 } },
    view: { x: 31, y: 48, z: 0.9 },
  };
  const layoutKey = "codex-canvas-graph:" + initial.stateDir;
  w.localStorage.setItem(layoutKey, JSON.stringify(layout));
  Object.defineProperty($("#canvas"), "clientWidth", { value: 1200 });
  Object.defineProperty($("#canvas"), "clientHeight", { value: 800 });
  for (const file of ["vendor/marked.js", "vendor/purify.js", "app.js"])
    w.eval(await readFile(join(skill, "web", file), "utf8"));
  await wait(
    () => $("#chat-list").querySelectorAll("[data-chat]").length === 2,
    "only two leads",
  );
  assert.equal(
    $("#chat-list").textContent.includes("Standalone reviewer"),
    false,
    "root worker is not a lead",
  );
  const release = [...$("#chat-list").querySelectorAll("button")].find((b) =>
    b.textContent.includes("Release lead"),
  );
  release.click();
  await wait(
    () => $("#messages").textContent.includes("I assigned 40"),
    "conversation",
  );
  assert.equal($("#workers").querySelectorAll("[data-worker]").length, 40);
  assert.equal($("#model").options.length, 2, "only Astra and Sol");
  assert.equal($("#messages").querySelector("script"), null, "safe transcript");
  assert.ok($("#messages .prose strong"), "markdown is rendered");
  assert.equal(
    $('#messages [href^="javascript:"]'),
    null,
    "unsafe markdown link removed",
  );
  assert.equal($("#messages img"), null, "remote images cannot make requests");
  assert.ok($("#messages .tool-group"), "tools stay in the conversation");
  assert.equal($("#messages .tool-group").open, false, "tools are folded");
  assert.equal($("#requests").querySelectorAll(".request").length, 1);
  $("#requests [data-answer]").click();
  $("#answer-fields select").value = "One file";
  $("#answer-form").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(() => !$("#answer-dialog").open, "async answer submitted");
  await wait(
    () => $("#requests").querySelectorAll(".request").length === 0,
    "async question resolved",
  );
  $("#message").value = "Lead draft";
  $("#worker-search").value = "Worker 07";
  $("#worker-search").dispatchEvent(new w.Event("input"));
  assert.equal($("#workers").querySelectorAll("[data-worker]").length, 1);
  $("#workers [data-worker]").click();
  await wait(
    () => $("#conversation-title").textContent === "Worker 07",
    "open worker",
  );
  $("#message").value = "Worker draft";
  $("#back-lead").click();
  assert.equal($("#message").value, "Lead draft");
  assert.deepEqual(
    JSON.parse(w.localStorage.getItem(layoutKey)),
    layout,
    "chat preserves stored canvas",
  );
  $("#view-toggle").click();
  assert.equal($("#canvas").hidden, false);
  assert.equal(
    $("#nodes").children.length,
    43,
    "canvas includes every team and standalone agent",
  );
  const node = [...$("#nodes").children].find(
    (n) => n.dataset.node === lead.id,
  );
  assert.equal(node.style.left, "517px");
  assert.equal(node.style.top, "319px");
  assert.equal(
    $("#canvas").querySelectorAll(":scope > button").length,
    1,
    "one canvas control",
  );
  assert.equal(
    $("#edges").querySelectorAll("path").length,
    40,
    "parent edges come from state",
  );
  $("#view-toggle").click();
  assert.equal($("#message").value, "Lead draft");
  $("#workers [data-worker]").click();
  assert.equal($("#message").value, "Worker draft");
  $("#back-lead").click();
  $("#new-chat").click();
  await wait(
    () => $("#toast").textContent === "Lost creation reply",
    "create failure visible",
  );
  assert.equal(
    w.document.querySelectorAll("#agent-form").length,
    0,
    "no creation form",
  );
  $("#new-chat").click();
  await wait(
    () => $("#conversation-title").textContent === "New chat",
    "one-click retry",
  );
  const snapshot = await (await fetch(origin + "/api/state")).json();
  const created = snapshot.runtime.agents.filter((a) => a.quickCreate);
  assert.equal(created.length, 1, "idempotent create");
  assert.equal(created[0].status, "idle");
  assert.equal(created[0].threadId, null, "no model turn until first message");
  $("#message").value = "Keep my draft";
  $("#new-chat").click();
  await new Promise((r) => setTimeout(r, 100));
  assert.equal(
    $("#message").value,
    "Keep my draft",
    "reuse empty chat preserves draft",
  );
  const reused = await (await fetch(origin + "/api/state")).json();
  assert.equal(
    reused.runtime.agents.filter((a) => a.quickCreate).length,
    1,
    "new chat reuses empty current chat",
  );
  const privateRoom = snapshot.runtime.rooms.find((r) => r.kind === "private");
  [...$("#agent-chat-list").querySelectorAll("[data-room]")]
    .find((b) => b.dataset.room === privateRoom.id)
    .click();
  await wait(
    () => $("#messages").textContent.includes("A private update"),
    "visible private chat",
  );
  assert.ok(
    $("#messages .message-label").textContent.includes("Worker 39"),
    "sender name visible",
  );
  assert.equal($("#composer").hidden, true, "user observes agent chat");
  assert.ok($("#earlier-messages"), "history has a previous page");
  $("#earlier-messages").click();
  await wait(
    () => $("#messages").querySelectorAll("[data-message]").length === 106,
    "earlier messages load",
  );
  await new Promise((r) => setTimeout(r, 1200));
  assert.equal(
    $("#earlier-messages"),
    null,
    "refresh preserves exhausted history cursor",
  );
  [...$("#chat-list").querySelectorAll("[data-chat]")]
    .find((b) => b.dataset.chat === created[0].id)
    .click();
  const forbidden = await fetch(origin + "/api/conversation", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      "X-Canvas-Token": snapshot.token,
    },
    body: JSON.stringify({ id: created[0].id, model: "gpt-5.6-luna" }),
  });
  assert.equal(forbidden.status, 400);
  $("#model").value = "gpt-5.6-sol";
  $("#model").dispatchEvent(new w.Event("change"));
  await wait(() => $("#model").value === "gpt-5.6-sol", "Sol selection");
  $("#message").value = "First user task";
  loseSend = true;
  $("#composer").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(
    () => $("#toast").textContent === "Lost message reply",
    "lost send visible",
  );
  [...$("#chat-list").querySelectorAll("button")]
    .find((b) => b.textContent.includes("Release lead"))
    .click();
  [...$("#chat-list").querySelectorAll("button")]
    .find((b) => b.textContent.includes("New chat"))
    .click();
  assert.equal($("#message").value, "First user task");
  $("#composer").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(() => !$("#message").value && !$("#send").disabled, "retry sent");
  const messages = await (
    await fetch(origin + "/api/messages?room=" + created[0].id)
  ).json();
  assert.equal(messages.length, 1, "no duplicate after switching");
  $("#message").value = "/monitor fixture-command";
  $("#composer").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(
    () => $("#monitors").textContent.includes("Exit: 7"),
    "monitor exit",
  );
  assert.match($("#monitors").textContent, /early output/);
  $('#conversation-menu [data-action="stop-team"]').click();
  await wait(
    () => $("#conversation-status").textContent === "Stopped",
    "stop team",
  );
  assert.equal($("#requests").querySelectorAll(".request").length, 0);
  $('#conversation-menu [data-action="delete"]').click();
  assert.equal($("#picker").open, true);
  $("#picker-body [data-delete-chat]").click();
  await wait(() => !$("#picker").open, "delete completes");
  const remaining = await (await fetch(origin + "/api/state")).json();
  assert.equal(
    remaining.runtime.agents.some((a) => a.id === created[0].id),
    false,
  );
  assert.equal($("#chat-list").querySelectorAll("[data-chat]").length, 2);
  console.log(
    "product UI: PASS (lead filter, 40 workers, Markdown safety, drafts, global canvas, instant create, model guard, retry, monitor, stop)",
  );
} finally {
  if (dom) {
    dom.window.dispatchEvent(new dom.window.Event("pagehide"));
    await new Promise((r) => setTimeout(r, 100));
    dom.window.close();
  }
  proc.kill("SIGTERM");
  if (proc.exitCode === null) await new Promise((r) => proc.once("exit", r));
  await rm(root, { recursive: true, force: true });
}
