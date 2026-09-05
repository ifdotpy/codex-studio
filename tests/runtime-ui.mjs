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
const fixture = `
import sys,importlib.util
from pathlib import Path
sys.dont_write_bytecode=True
sys.path.insert(0,sys.argv[1]+'/scripts')
from codex_canvas import Canvas,make_server
from codex_runtime import Runtime
spec=importlib.util.spec_from_file_location('fixture',sys.argv[1]+'/tests/runtime-contract.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
c=Canvas(Path(sys.argv[2]));c.runtime=Runtime(c.root,m.FakeServer)
s=c.runtime.connect();s.finish_before_reply=True;s.gate.set()
server=make_server(c)
print(server.server_port,flush=True)
server.serve_forever()
`;
const proc = spawn("python3", ["-B", "-c", fixture, skill, root], {
  stdio: ["ignore", "pipe", "pipe"],
});
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
  let loseCreate = true;
  w.fetch = async (url, options) => {
    const result = await fetch(new URL(url, origin), options);
    if (url === "/api/agents" && loseCreate) {
      loseCreate = false;
      throw new Error("Lost creation reply");
    }
    return result;
  };
  Object.defineProperty($("#viewport"), "clientWidth", { value: 1400 });
  Object.defineProperty($("#viewport"), "clientHeight", { value: 800 });
  w.eval(await readFile(join(skill, "web/app.js"), "utf8"));
  await wait(() => $("#connection").textContent.includes("Live"), "state");
  $("#new-agent").click();
  await wait(() => $("#agent-model").options.length === 2, "models");
  $("#agent-name").value = "Managed lead";
  $("#agent-cwd").value = root;
  $("#agent-task").value = "Coordinate reviewers";
  $("#agent-concurrency").value = "4";
  $("#agent-form").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(() => !$("#agent-form-error").hidden, "lost response visible");
  $("#agent-form").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(() => !$("#agent-dialog").open, "idempotent creation retry");
  await wait(
    () => $("#detail-title").textContent === "Managed lead",
    "lead inspector",
  );
  assert.equal(
    w.document.querySelectorAll(".agent-card").length,
    1,
    "no duplicate agent",
  );
  assert.equal($("#agent-actions").hidden, false);
  const snapshot = await (await fetch(origin + "/api/state")).json();
  assert.equal(snapshot.runtime.agents[0].concurrency, 4);
  $("#watch-command").click();
  $("#monitor-command").value = "fixture-command";
  $("#monitor-form").dispatchEvent(
    new w.Event("submit", { bubbles: true, cancelable: true }),
  );
  await wait(
    () => $("#detail-body").textContent.includes("Exit: 7"),
    "monitor exit visible",
  );
  assert.match($("#detail-body").textContent, /early output/);
  assert.match($("#detail-body").textContent, /monitor_exit/);
  $("#detail-body [data-stop-root]").click();
  await wait(
    () => $("#detail-body").textContent.includes("Stopped by user"),
    "stop",
  );
  const after = await (await fetch(origin + "/api/state")).json();
  assert.equal(after.runtime.agents[0].autoWake, false);
  assert.equal(after.runtime.agents[0].status, "paused");
  console.log(
    "managed runtime UI: PASS (models, create, lost reply deduplication, monitor, exit event, stop team)",
  );
} finally {
  dom?.window.close();
  proc.kill("SIGTERM");
  await new Promise((r) => proc.once("exit", r));
  await rm(root, { recursive: true, force: true });
}
