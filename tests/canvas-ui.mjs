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
rows=[dict(name=n,threadId=n,runId='fixture',turnStatus='running',goalStatus='active',launcherPid=os.getpid(),boardOwner='ui:fixture:'+n,tail='Read <script> safely',tokensUsed=12345,events=99) for n in ['alpha','beta']]
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
  const lostReplies = new Set(["/api/groups", "/api/messages"]);
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
  await wait(
    () => document.querySelectorAll(".agent-card").length === 2,
    "agents appear",
  );
  assert.equal($("#error").hidden, true);
  assert.equal(document.querySelectorAll(".agent-card script").length, 0);
  assert.match($(".card-tail").textContent, /<script>/);
  const alpha = document.querySelector('[aria-label^="alpha,"]');
  const beta = document.querySelector('[aria-label^="beta,"]');
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
  for (const el of [alpha, beta])
    el.dispatchEvent(
      new w.KeyboardEvent("keydown", {
        key: " ",
        shiftKey: true,
        bubbles: true,
      }),
    );
  assert.equal($("#selection-count").textContent, "2 selected");
  $("#new-group").click();
  assert.equal($("#group-dialog").open, true);
  $("#group-name").value = "Fixture team";
  const groupButton = $('#group-form button[type="submit"]');
  $("#group-form").dispatchEvent(
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
  $("#group-form").dispatchEvent(
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
  const groups = snapshot.groups.filter((g) => !g.automatic);
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
  $("#close-detail").click();
  $("#chats").click();
  assert.match($("#detail-body").textContent, /Fixture team/);
  $("#close-detail").click();
  $("#search").value = "absent";
  $("#search").dispatchEvent(new w.Event("input"));
  assert.equal($("#empty").hidden, false);
  assert.equal($("#empty h2").textContent, "No matching agents");
  $("#clear-filter").click();
  assert.equal(document.querySelectorAll(".agent-card").length, 2);
  $("#resources").click();
  assert.match($("#detail-body").textContent, /No resource claims/);
  console.log(
    "canvas DOM integration: PASS (drag, layout persistence, transcript, XSS text, group, mailbox, lost-response retries, live peer reply, chat list, filters, resources)",
  );
} finally {
  dom?.window.close();
  proc.kill("SIGTERM");
  if (proc.exitCode === null)
    await new Promise((resolve) => proc.once("exit", resolve));
  await rm(root, { recursive: true, force: true });
}
