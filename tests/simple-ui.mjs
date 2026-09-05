#!/usr/bin/env node
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
const root = await mkdtemp(join(tmpdir(), "codex-runtime-ui-"));
const proc = spawn(
  "python3",
  ["-B", join(skill, "tests/simple-ui-fixture.py"), root],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let errors = "",
  dom;
proc.stderr.on("data", (d) => (errors += d));
const wait = async (fn, label) => {
  const end = Date.now() + 8000;
  while (Date.now() < end) {
    if (fn()) return;
    await new Promise((r) => setTimeout(r, 20));
  }
  throw new Error(label + " " + errors);
};
try {
  const port = await new Promise((resolve, reject) => {
    proc.stdout.once("data", (d) => resolve(Number(String(d).trim())));
    proc.once("exit", () => reject(new Error(errors)));
  });
  const origin = `http://127.0.0.1:${port}`;
  dom = new JSDOM(await readFile(join(skill, "web/index.html"), "utf8"), {
    url: origin,
    runScripts: "outside-only",
    pretendToBeVisual: true,
  });
  const w = dom.window,
    $ = (s) => w.document.querySelector(s);
  w.HTMLCanvasElement.prototype.getContext = () => ({
    clearRect() {},
    fillRect() {},
    strokeRect() {},
  });
  w.HTMLElement.prototype.setPointerCapture = () => {};
  w.ResizeObserver = class {
    observe() {}
  };
  w.HTMLDialogElement.prototype.showModal = function () {
    this.open = true;
  };
  w.HTMLDialogElement.prototype.close = function () {
    this.open = false;
  };
  w.crypto.randomUUID = randomUUID;
  let loseSend = false;
  w.fetch = async (url, options) => {
    const response = await fetch(new URL(url, origin), options);
    if (url === "/api/messages" && options?.method === "POST" && loseSend) {
      loseSend = false;
      throw new Error("Lost message reply");
    }
    return response;
  };
  Object.defineProperty($("#viewport"), "clientWidth", { value: 1400 });
  Object.defineProperty($("#viewport"), "clientHeight", { value: 800 });
  const initial = await (await fetch(origin + "/api/state")).json();
  const leadId = initial.runtime.agents.find(
    (a) => a.name === "Release lead",
  ).id;
  const storedLayout = {
    positions: { [leadId]: { x: 517, y: 319 } },
    view: { x: 31, y: 48, z: 0.9 },
  };
  const layoutKey = "codex-canvas-graph:" + initial.stateDir;
  w.localStorage.setItem(layoutKey, JSON.stringify(storedLayout));
  w.eval(await readFile(join(skill, "web/app.js"), "utf8"));
  await wait(() => $("#connection").textContent.includes("Live"), "state");
  assert.equal(w.document.body.classList.contains("simple-view"), true);
  await wait(() => $("#worker-count").textContent === "40", "40 workers");
  await wait(
    () => $("#detail-body").textContent.includes("I assigned 40"),
    "lead conversation",
  );
  assert.equal($("#conversation-options").open, false);
  assert.equal(
    $("#detail-body").textContent.includes("Hidden tool fixture"),
    false,
  );
  assert.equal($("#team-select").options.length, 2);
  assert.equal($("#worker-list").querySelectorAll("button").length, 40);
  assert.match($("#worker-list button").textContent, /Worker 07/);
  $("#message").value = "Draft for the lead";
  const rootId = $("#team-select").value;
  $("#worker-search").value = "Worker 07";
  $("#worker-search").dispatchEvent(new w.Event("input"));
  assert.equal($("#worker-list").querySelectorAll("button").length, 1);
  $("#worker-list button").click();
  await wait(
    () => $("#detail-title").textContent === "Worker 07",
    "worker selection",
  );
  await wait(
    () => $("#detail-body").textContent.includes("Worker 07 report"),
    "worker transcript",
  );
  assert.equal($("#detail-body script"), null, "escape transcript text");
  $("#message").value = "Draft for worker";
  $("#open-lead").click();
  assert.equal($("#message").value, "Draft for the lead");
  assert.deepEqual(
    JSON.parse(w.localStorage.getItem(layoutKey)),
    storedLayout,
    "chat does not rewrite saved graph",
  );
  $("#advanced-view").click();
  assert.equal(
    w.document.getElementById("agent-" + leadId).style.left,
    "517px",
  );
  assert.equal(w.document.getElementById("agent-" + leadId).style.top, "319px");
  assert.equal(w.document.body.classList.contains("simple-view"), false);
  assert.equal(w.document.querySelectorAll(".agent-card").length, 42);
  const card = w.document.querySelector(".agent-card");
  const position = [card.style.left, card.style.top];
  $("#simple-view").click();
  assert.equal($("#message").value, "Draft for the lead");
  $("#advanced-view").click();
  assert.deepEqual([card.style.left, card.style.top], position);
  $("#simple-view").click();
  $("#worker-list button").click();
  assert.equal($("#message").value, "Draft for worker");
  $("#simple-operations").click();
  await wait(
    () => $("#detail-title").textContent === "Team operations",
    "operations",
  );
  $("#open-lead").click();
  assert.equal($("#message").value, "Draft for the lead");
  $("#message").value = "Follow up from the simple view";
  loseSend = true;
  $("#composer").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(
    () => $("#toast").textContent === "Lost message reply",
    "lost send response",
  );
  $("#worker-list button").click();
  $("#open-lead").click();
  assert.equal($("#message").value, "Follow up from the simple view");
  $("#composer").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(() => !$("#message").value, "message retry");
  assert.equal(
    w.document
      .querySelector('[data-tab="activity"]')
      .classList.contains("selected"),
    true,
  );
  const messages = await (
    await fetch(origin + "/api/messages?room=" + rootId)
  ).json();
  assert.equal(messages.length, 1, "switching agents preserves retry identity");
  assert.equal(messages.at(-1).text, "Follow up from the simple view");
  $("#team-select").value = [...$("#team-select").options].find(
    (o) => o.value !== rootId,
  ).value;
  $("#team-select").dispatchEvent(new w.Event("change"));
  assert.equal($("#worker-count").textContent, "0", "team isolation");
  assert.match($("#worker-list").textContent, /Workers appear here/);
  console.log(
    "simple workspace: PASS (40 workers, search, team scope, drafts, send, tools, canvas round trip)",
  );
} finally {
  dom?.window.document.querySelector("#close-detail").click();
  dom?.window.close();
  proc.kill("SIGTERM");
  await new Promise((r) => proc.once("exit", r));
  await rm(root, { recursive: true, force: true });
}
