#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import {
  createCommandTransport,
  commandEnvironment,
} from "../scripts/claude_bridge/commands.mjs";

const root = await fs.mkdtemp(
  path.join(os.tmpdir(), "studio-claude-commands-"),
);
const transports = [];
const transport = (options) => {
  const value = createCommandTransport(options);
  transports.push(value);
  return value;
};
async function until(predicate, label) {
  const deadline = Date.now() + 5000;
  while (!predicate()) {
    if (Date.now() >= deadline) throw new Error("Timed out: " + label);
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
}
const alive = (pid) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    if (error.code === "ESRCH") return false;
    throw error;
  }
};

try {
  assert.deepEqual(
    commandEnvironment(
      {
        HOME: "/home",
        PATH: "/bin",
        LANG: "en_US.UTF-8",
        OPENAI_API_KEY: "secret",
        CODEX_API_KEY: "secret",
        ANTHROPIC_API_KEY: "secret",
        CLAUDE_CODE_OAUTH_TOKEN: "secret",
        AWS_SECRET_ACCESS_KEY: "secret",
        NODE_OPTIONS: "--require bad.js",
        CODEX_HOME: "/original",
      },
      "/isolated",
    ),
    {
      HOME: "/home",
      PATH: "/bin",
      LANG: "en_US.UTF-8",
      CODEX_HOME: "/isolated",
    },
  );

  const events = [];
  const native = transport({
    root: path.join(root, "native"),
    emit: (method, params) => events.push({ method, ...params }),
  });
  const params = (processId, script, extra = {}) => ({
    processId,
    command: ["/bin/sh", "-c", script],
    cwd: root,
    sandboxPolicy: { type: "readOnly" },
    timeoutMs: 5000,
    streamStdoutStderr: true,
    ...extra,
  });
  const output = (id, stream = "stdout") =>
    events
      .filter((event) => event.processId === id && event.stream === stream)
      .map((event) => Buffer.from(event.deltaBase64, "base64").toString())
      .join("");

  await assert.rejects(
    native.handle("command/exec", {
      processId: "no-policy",
      command: ["/bin/true"],
    }),
    /explicit native sandboxPolicy/,
  );
  await assert.rejects(
    native.handle(
      "command/exec",
      params("external", "true", {
        sandboxPolicy: { type: "externalSandbox" },
      }),
    ),
    /no external sandbox/,
  );
  await assert.rejects(native.handle("account/read", {}), /Unsupported/);
  await assert.rejects(fs.stat(path.join(root, "native")), { code: "ENOENT" });
  const config = await native.handle("config/read", {
    cwd: root,
    includeLayers: false,
  });
  assert.equal(typeof config.config, "object");
  assert.deepEqual(
    await native.handle(
      "command/exec",
      params("output", "printf hello; printf error >&2"),
    ),
    { exitCode: 0, stdout: "", stderr: "" },
  );
  assert.equal(output("output"), "hello");
  assert.equal(output("output", "stderr"), "error");
  assert(events.every((event) => event.method === "command/exec/outputDelta"));
  await assert.rejects(
    native.handle("command/exec", params("output", "echo repeated")),
    /already submitted or reserved/,
  );

  const restrictedPath = path.join(root, "read-only-denied");
  const denied = await native.handle(
    "command/exec",
    params("denied", 'printf forbidden > "$1"', {
      command: [
        "/bin/sh",
        "-c",
        'printf forbidden > "$1"',
        "test",
        restrictedPath,
      ],
    }),
  );
  assert.notEqual(denied.exitCode, 0);
  assert.match(output("denied", "stderr"), /not permitted|denied|read.only/i);
  await assert.rejects(fs.stat(restrictedPath), { code: "ENOENT" });
  const writablePath = path.join(root, "workspace-allowed");
  assert.equal(
    (
      await native.handle(
        "command/exec",
        params("allowed", "", {
          command: [
            "/bin/sh",
            "-c",
            'printf allowed > "$1"',
            "test",
            writablePath,
          ],
          sandboxPolicy: {
            type: "workspaceWrite",
            writableRoots: [root],
            networkAccess: false,
          },
        }),
      )
    ).exitCode,
    0,
  );
  assert.equal(await fs.readFile(writablePath, "utf8"), "allowed");

  const pty = native.handle(
    "command/exec",
    params(
      "pty",
      'printf READY; read answer; stty size; printf "ANSWER:%s" "$answer"',
      { tty: true, streamStdin: true },
    ),
  );
  await until(() => output("pty").includes("READY"), "PTY ready");
  await native.handle("command/exec/resize", {
    processId: "pty",
    size: { rows: 31, cols: 91 },
  });
  await native.handle("command/exec/write", {
    processId: "pty",
    deltaBase64: Buffer.from("hello\n").toString("base64"),
    closeStdin: false,
  });
  assert.equal((await pty).exitCode, 0);
  assert.match(output("pty"), /31 91/);
  assert.match(output("pty"), /ANSWER:hello/);

  const stdin = native.handle(
    "command/exec",
    params("stdin", "printf READY; cat; printf CLOSED", { streamStdin: true }),
  );
  await until(() => output("stdin").includes("READY"), "stdin ready");
  await native.handle("command/exec/write", {
    processId: "stdin",
    deltaBase64: Buffer.from("pipe input").toString("base64"),
    closeStdin: true,
  });
  assert.equal((await stdin).exitCode, 0);
  assert.equal(output("stdin"), "READYpipe inputCLOSED");

  const cancel = native.handle(
    "command/exec",
    params("cancel", "echo $$; exec sleep 30"),
  );
  await until(() => /^\d+\n$/.test(output("cancel")), "cancel PID");
  const cancelledPid = Number(output("cancel").trim());
  await native.handle("command/exec/terminate", { processId: "cancel" });
  assert.notEqual((await cancel).exitCode, 0);
  await until(() => !alive(cancelledPid), "cancelled process exit");
  assert.equal(
    (
      await native.handle(
        "command/exec",
        params("timeout", "sleep 30", { timeoutMs: 100 }),
      )
    ).exitCode,
    124,
  );

  // Close uses native cancellation even when a command owns its own process group.
  const closing = native.handle(
    "command/exec",
    params("close", "echo $$; exec sleep 30"),
  );
  const closedResult = closing.then(
    (result) => result,
    (error) => error,
  );
  await until(() => /^\d+\n$/.test(output("close")), "close PID");
  const closedPid = Number(output("close").trim());
  await native.close();
  const terminal = await closedResult;
  assert(terminal instanceof Error || terminal.exitCode !== 0);
  await until(() => !alive(closedPid), "closed process exit");
  const reopened = transport({
    root: path.join(root, "native"),
    emit: () => {},
  });
  await assert.rejects(
    reopened.handle("command/exec", params("output", "echo repeated")),
    /already submitted or reserved/,
  );

  // A bridge EOF or SIGTERM must stop both native processes without a model call.
  const runner = path.join(root, "lifecycle.mjs");
  await fs.writeFile(
    runner,
    `
import {createCommandTransport} from ${JSON.stringify(new URL("../scripts/claude_bridge/commands.mjs", import.meta.url).href)};
const t = createCommandTransport({root:process.argv[2],emit:(method,p)=>process.stdout.write(Buffer.from(p.deltaBase64,"base64"))});
let stopping;
function stop() { stopping ||= t.close().then(()=>process.exit(0)); }
process.stdin.resume(); process.stdin.on("end",stop); process.on("SIGTERM",stop);
t.handle("command/exec",{processId:"lifecycle",command:["/bin/sh","-c","echo $$ $PPID; exec sleep 30"],cwd:process.argv[2],sandboxPolicy:{type:"readOnly"},streamStdoutStderr:true,timeoutMs:30000}).catch(()=>{});
`,
  );
  for (const action of ["eof", "sigterm"]) {
    const fixtureState = path.join(root, "lifecycle-" + action);
    await fs.mkdir(fixtureState);
    const bridge = spawn(process.execPath, [runner, fixtureState], {
      stdio: ["pipe", "pipe", "pipe"],
    });
    let received = "",
      errors = "";
    bridge.stdout.on("data", (value) => {
      received += value;
    });
    bridge.stderr.on("data", (value) => {
      errors += value;
    });
    const exit = new Promise((resolve) =>
      bridge.on("exit", (code) => resolve(code)),
    );
    try {
      await until(
        () => /^\d+ \d+\n$/.test(received),
        "bridge child identities",
      );
      const [commandPid, executorPid] = received.trim().split(" ").map(Number);
      if (action === "eof") bridge.stdin.end();
      else bridge.kill("SIGTERM");
      assert.equal(await exit, 0, errors);
      await until(
        () => !alive(commandPid) && !alive(executorPid),
        action + " descendants exit",
      );
    } finally {
      if (bridge.exitCode === null) bridge.kill("SIGTERM");
    }
  }

  // A protocol fixture drops the receipt after accepting exactly one command.
  const fake = path.join(root, "fake-codex");
  const journal = path.join(root, "fake-requests.jsonl");
  await fs.writeFile(
    fake,
    `#!${process.execPath}\n` +
      `
const fs = require("node:fs");
require("node:readline").createInterface({input:process.stdin}).on("line", (line) => {
  const m = JSON.parse(line); fs.appendFileSync(${JSON.stringify(journal)}, line + "\\n");
  if (m.method === "initialized") return;
  if (m.method === "command/exec") { process.exit(3); return; }
  const response = m.method === "command/exec/write"
    ? {id:m.id,error:{code:-32602,message:"native failure",data:{reason:"exact"}}}
    : {id:m.id,result:{config:{}}};
  process.stdout.write(JSON.stringify(response)+"\\n");
});
`,
    { mode: 0o700 },
  );
  const fixtureRoot = path.join(root, "fixture");
  const fixture = transport({
    root: fixtureRoot,
    env: { ...process.env, CODEX_BIN: fake },
    emit: () => {},
  });
  await Promise.all([
    fixture.handle("config/read", {}),
    fixture.handle("config/read", {}),
  ]);
  await assert.rejects(
    fixture.handle("command/exec/write", { processId: "missing" }),
    (error) => {
      assert.equal(error.code, -32602);
      assert.equal(error.message, "native failure");
      assert.deepEqual(error.data, { reason: "exact" });
      return true;
    },
  );
  await assert.rejects(
    fixture.handle("command/exec", params("lost-reply", "true")),
    /outcome unknown/,
  );
  await assert.rejects(
    fixture.handle("command/exec", params("lost-reply", "true")),
    /outcome unknown/,
  );
  const reconnect = transport({
    root: fixtureRoot,
    env: { ...process.env, CODEX_BIN: fake },
    emit: () => {},
  });
  await assert.rejects(
    reconnect.handle("command/exec", params("lost-reply", "true")),
    /already submitted or reserved/,
  );
  const requests = (await fs.readFile(journal, "utf8"))
    .trim()
    .split("\n")
    .map(JSON.parse);
  assert.equal(
    requests.filter((entry) => entry.method === "initialize").length,
    2,
  );
  assert.equal(
    requests.filter((entry) => entry.method === "command/exec").length,
    1,
  );
  const ids = requests
    .filter((entry) => entry.id !== undefined)
    .map((entry) => entry.id);
  assert.equal(new Set(ids).size, ids.length);
  assert(
    requests.every((entry) =>
      [
        "initialize",
        "initialized",
        "config/read",
        "command/exec",
        "command/exec/write",
      ].includes(entry.method),
    ),
  );
  console.log(
    "PASS: native sandbox, streams, PTY, stdin close, cancel, timeout, bridge EOF and SIGTERM cleanup; credential isolation, exact errors, unique IDs, persistent no-replay",
  );
} finally {
  await Promise.allSettled(transports.map((value) => value.close()));
  await fs.rm(root, { recursive: true, force: true });
}
