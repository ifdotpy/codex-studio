#!/usr/bin/env node
// DOM integration against a fixture server. No browser rendering or model inference.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import { randomUUID } from "node:crypto";
const skill = dirname(dirname(fileURLToPath(import.meta.url)));
const require = createRequire(join(skill, "web/package.json"));
const { JSDOM } = require("jsdom");
const root = await mkdtemp(join(tmpdir(), "codex-canvas-ui-"));
const fixture = `
import sys,os,json
from pathlib import Path
sys.dont_write_bytecode=True
sys.path.insert(0,sys.argv[1])
from codex_canvas import Canvas,make_server
c=Canvas()
rows=[dict(name=n,threadId=n,runId='fixture',turnStatus='running',goalStatus='active',launcherPid=os.getpid(),boardOwner='ui:fixture:'+n,tail='Read <script> safely',tokensUsed=12345,events=99,orchestratorId='fixture-lead',orchestratorName='Fixture lead') for n in ['alpha','beta']]
(c.root/'codex-swarm-status.ui.json').write_text(json.dumps(rows))
p=Path(os.environ['CODEX_HOME'])/'sessions/2026/09/05'
p.mkdir(parents=True)
(p/'rollout-alpha.jsonl').write_text(json.dumps(dict(type='response_item',payload=dict(type='message',role='assistant',content=[dict(type='output_text',text='Live fixture message <script>')])))+'\\n')
s=make_server(c)
print(s.server_port,flush=True)
s.serve_forever()
`;
const proc = spawn("python3", ["-B", "-c", fixture, join(skill, "scripts")], {
  env: {
    ...process.env,
    CODEX_AGENTS_STATE_DIR: join(root, "state"),
    CODEX_HOME: join(root, "profile"),
    CODEX_BOARD_STATE_DIR: "",
  },
  stdio: ["ignore", "pipe", "pipe"],
});
let errors = "";
proc.stderr.on("data", (d) => (errors += d));
let dom;
const wait = async (fn, label) => {
  const end = Date.now() + 5000;
  while (Date.now() < end) {
    if (fn()) return;
    await new Promise((r) => setTimeout(r, 20));
  }
  throw new Error("Timeout: " + label + " " + errors);
};
try {
  const port = await new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("Fixture startup timeout: " + errors)),
      5000,
    );
    proc.stdout.once("data", (d) => {
      clearTimeout(timer);
      resolve(Number(String(d).trim()));
    });
    proc.once("exit", () => {
      clearTimeout(timer);
      reject(new Error(errors));
    });
  });
  const origin = `http://127.0.0.1:${port}`;
  const html = await readFile(join(skill, "web/index.html"), "utf8");
  dom = new JSDOM(html, {
    url: origin,
    runScripts: "outside-only",
    pretendToBeVisual: true,
  });
  const w = dom.window,
    document = w.document;
  const $ = (s) => document.querySelector(s);
  const style = document.createElement("style");
  style.textContent = await readFile(join(skill, "web/style.css"), "utf8");
  document.head.append(style);
  const context = { clearRect() {}, fillRect() {}, strokeRect() {} };
  w.HTMLCanvasElement.prototype.getContext = () => context;
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
  const lostReplies = new Set(["/api/chats", "/api/messages"]);
  w.fetch = async (url, options) => {
    const response = await fetch(new URL(url, origin), options);
    if (options?.method === "POST" && lostReplies.delete(url)) {
      throw new Error("Fixture lost response");
    }
    return response;
  };
  Object.defineProperty($("#viewport"), "clientWidth", { value: 1400 });
  Object.defineProperty($("#viewport"), "clientHeight", { value: 800 });
  w.eval(await readFile(join(skill, "web/app.js"), "utf8"));
  $("#advanced-view").click();
  await wait(
    () => document.querySelectorAll(".agent-card").length === 3,
    "agents appear",
  );
  assert.equal($("#error").hidden, true);
  assert.equal(document.querySelectorAll(".agent-card script").length, 0);
  const alpha = document.querySelector('[aria-label^="alpha,"]');
  const beta = document.querySelector('[aria-label^="beta,"]');
  assert.match(alpha.querySelector(".card-tail").textContent, /<script>/);
  assert.equal(w.getComputedStyle(alpha).width, "208px");
  assert.equal(w.getComputedStyle(alpha).height, "76px");
  assert.equal(document.querySelectorAll(".connection.spawn").length, 2);
  const pointer = (element, type, x, y) => {
    const e = new w.Event(type, { bubbles: true });
    Object.assign(e, { clientX: x, clientY: y, pointerId: 1, button: 0 });
    element.dispatchEvent(e);
  };
  const oldLeft = parseFloat(alpha.style.left);
  pointer(alpha, "pointerdown", 100, 100);
  pointer($("#viewport"), "pointermove", 185, 100);
  pointer($("#viewport"), "pointerup", 185, 100);
  assert.equal(parseFloat(alpha.style.left), oldLeft + 100);
  await new Promise((r) => setTimeout(r, 250));
  const saved = JSON.parse(w.localStorage.getItem(w.localStorage.key(0)));
  assert.equal(saved.positions[alpha.dataset.id].x, oldLeft + 100);
  alpha.dispatchEvent(
    new w.KeyboardEvent("keydown", { key: "Enter", bubbles: true }),
  );
  await wait(
    () => $("#detail-body").textContent.includes("Live fixture message"),
    "live transcript",
  );
  assert.equal($("#inspector").hidden, false);
  assert.equal(document.querySelectorAll("#detail-body script").length, 0);
  $("#close-detail").click();
  $("#clear-selection").click();
  pointer(alpha, "pointerdown", 100, 100);
  pointer($("#viewport"), "pointerup", 100, 100);
  assert.equal($("#inspector").hidden, true, "a click selects without opening");
  assert.equal($("#selection-count").textContent, "1 selected");
  $("#clear-selection").click();
  const z = parseInt($("#zoom").textContent) / 100;
  const minX =
    Math.min(parseFloat(alpha.style.left), parseFloat(beta.style.left)) * z +
    45 -
    5;
  const maxX =
    (Math.max(parseFloat(alpha.style.left), parseFloat(beta.style.left)) +
      208) *
      z +
    45 +
    5;
  const y = parseFloat(alpha.style.top) * z + 45;
  pointer($("#viewport"), "pointerdown", minX, y - 5);
  pointer($("#viewport"), "pointermove", maxX, y + 76 * z + 5);
  pointer($("#viewport"), "pointerup", maxX, y + 76 * z + 5);
  assert.equal($("#selection-count").textContent, "2 selected");
  const before = [alpha, beta].map((el) => parseFloat(el.style.left));
  const beforeEdges = [...document.querySelectorAll(".connection.spawn")].map(
    (p) => p.getAttribute("d"),
  );
  pointer(alpha, "pointerdown", 100, 100);
  pointer($("#viewport"), "pointermove", 185, 100);
  pointer($("#viewport"), "pointerup", 185, 100);
  assert.deepEqual(
    [alpha, beta].map((el) => parseFloat(el.style.left)),
    before.map((x) => x + 100),
  );
  assert.notDeepEqual(
    [...document.querySelectorAll(".connection.spawn")].map((p) =>
      p.getAttribute("d"),
    ),
    beforeEdges,
  );
  $("#new-chat").click();
  assert.equal($("#chat-dialog").open, true);
  $("#chat-name").value = "Fixture team";
  const groupButton = $('#chat-form button[type="submit"]');
  $("#chat-form").dispatchEvent(
    new w.SubmitEvent("submit", {
      bubbles: true,
      cancelable: true,
      submitter: groupButton,
    }),
  );
  await wait(
    () => $("#toast").textContent === "Fixture lost response",
    "lost group response",
  );
  $("#chat-form").dispatchEvent(
    new w.SubmitEvent("submit", {
      bubbles: true,
      cancelable: true,
      submitter: groupButton,
    }),
  );
  await wait(
    () => $("#detail-title").textContent === "Fixture team",
    "group opens",
  );
  assert.equal(document.querySelectorAll(".chat-node").length, 1);
  assert.equal(document.querySelectorAll(".connection.chat").length, 2);
  $("#message").value = "Inspect the fixture";
  const send = () =>
    $("#composer").dispatchEvent(
      new w.SubmitEvent("submit", {
        bubbles: true,
        cancelable: true,
        submitter: $("#composer button"),
      }),
    );
  send();
  await wait(
    () => $("#toast").textContent === "Fixture lost response",
    "lost message response",
  );
  send();
  await wait(
    () => $("#detail-body").textContent.includes("Inspect the fixture"),
    "group message appears",
  );
  assert.match($("#detail-body").textContent, /Queued in agent mailbox/);
  const snapshot = await (await fetch(origin + "/api/state")).json();
  const groups = snapshot.chats;
  assert.equal(
    groups.length,
    1,
    "lost group response must not create duplicates",
  );
  const room = groups[0].id;
  const messages = await (
    await fetch(origin + "/api/messages?room=" + room)
  ).json();
  assert.equal(
    messages.length,
    1,
    "lost message response must not duplicate an instruction",
  );
  const reply = spawnSync(
    join(skill, "scripts/codex-chat"),
    ["post", room, "Peer reply from alpha", "--owner", "ui:fixture:alpha"],
    {
      env: {
        ...process.env,
        CODEX_AGENTS_STATE_DIR: join(root, "state"),
        CODEX_HOME: join(root, "profile"),
        CODEX_BOARD_STATE_DIR: "",
      },
      encoding: "utf8",
      timeout: 5000,
    },
  );
  assert.equal(reply.status, 0, reply.stderr);
  await wait(
    () => $("#detail-body").textContent.includes("Peer reply from alpha"),
    "live peer reply",
  );
  const alphaEdge = [...document.querySelectorAll(".connection.chat")].find(
    (p) => p.getAttribute("aria-label").startsWith("alpha "),
  );
  alphaEdge.dispatchEvent(new w.MouseEvent("click", { bubbles: true }));
  assert.equal($("#disconnect").hidden, false);
  $("#disconnect").click();
  await wait(
    () => document.querySelectorAll(".connection.chat").length === 1,
    "disconnect edge",
  );
  const retained = await (
    await fetch(origin + "/api/messages?room=" + room)
  ).json();
  assert.equal(retained.length, 2, "disconnect preserves messages");
  alpha.querySelector("[data-port]").click();
  $(".chat-node [data-port]").click();
  await wait(
    () => document.querySelectorAll(".connection.chat").length === 2,
    "reconnect through ports",
  );
  $("#close-detail").click();
  $("#chats").click();
  assert.match($("#detail-body").textContent, /Fixture team/);
  $("#close-detail").click();
  $("#search").value = "absent";
  $("#search").dispatchEvent(new w.Event("input"));
  assert.equal($("#empty").hidden, false);
  assert.equal($("#empty h2").textContent, "No matching nodes");
  $("#clear-filter").click();
  assert.equal(document.querySelectorAll(".agent-card").length, 4);
  $("#resources").click();
  assert.match($("#detail-body").textContent, /No resource claims/);
  console.log(
    "canvas DOM integration: PASS (compact nodes, selection, marquee, multi-drag, creator edges, chat nodes, connect, disconnect, history, mailbox retries, live replies, filters)",
  );
} finally {
  dom?.window.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
  await rm(root, { recursive: true, force: true });
}
