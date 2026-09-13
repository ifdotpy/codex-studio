#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync, symlinkSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const scripts = join(dirname(dirname(fileURLToPath(import.meta.url))), "scripts");
const root = mkdtempSync(join(tmpdir(), "codex-state-contract-"));
const state = join(root, "state");
const profile = join(root, "profile");
mkdirSync(state);
const env = { ...process.env, CODEX_AGENTS_STATE_DIR: state, CODEX_HOME: profile };
const json = (path, value) => writeFileSync(path, JSON.stringify(value));
function run(name, args, extra = {}, status = 0) {
  const result = spawnSync(join(scripts, name), args, {
    env: { ...env, ...extra }, encoding: "utf8", timeout: 10000,
  });
  assert.equal(result.status, status, name + ": " + result.stdout + result.stderr);
  return result.stdout + result.stderr;
}

let passed = false;
try {
  for (const wave of ["old.wave", "new"]) {
    json(join(state, "codex-swarm-status." + wave + ".json"), [{
      wave, name: "worker", threadId: "thread-" + wave, runId: "run-" + wave,
      launcherPid: process.pid, turnStatus: "running", goalStatus: "active",
      lastEvent: new Date().toISOString(), boardOwner: wave + ":run:worker",
    }]);
    writeFileSync(join(state, "codex-swarm-launcher." + wave + ".pid"), String(process.pid));
  }
  const list = run("luna", ["ls", "--all"]);
  assert.equal((list.match(/running/g) || []).length, 2);
  assert.doesNotMatch(list, /abandoned/);
  assert.match(run("codex-daemon", ["status", "--wave", "old.wave"]), /old\.wave: launcher/);
  assert.match(run("luna", ["say", "worker", "ambiguous"], {}, 1), /ambiguous|multiple/);
  run("luna", ["say", "--wave", "old.wave", "worker", "delivered"]);
  const inbox = join(state, "codex-inbox.old.wave.run-old.wave");
  assert.deepEqual(readdirSync(inbox), ["worker.json"]);
  assert.equal(JSON.parse(readFileSync(join(inbox, "worker.json"))).text, "delivered\n");
  assert.match(run("luna", ["say", "--wave", "old.wave", "worker", "second"], {}, 1), /pending message/);
  run("luna", ["say", "--wave", "new", "worker", "--", "--wave", "old.wave", "literal"]);
  const routed = join(state, "codex-inbox.new.run-new", "worker.json");
  assert.equal(JSON.parse(readFileSync(routed)).text, "--wave old.wave literal\n");
  // Historical claims cannot block the dashboard or change preserved state.
  const legacyBoard = join(state, "board", "codex-board.json");
  mkdirSync(join(state, "board"));
  writeFileSync(legacyBoard, "{invalid historical data");
  assert.match(run("luna", []), /live launchers/);
  assert.equal(readFileSync(legacyBoard, "utf8"), "{invalid historical data");
  assert.match(run("luna", ["board"], {}, 2), /invalid choice/);

  const sessions = join(profile, "sessions", "2026", "09", "04");
  mkdirSync(sessions, { recursive: true });
  json(join(sessions, "rollout-thread-old.wave.jsonl"), { payload: {
    role: "assistant", phase: "final_answer", content: [{ type: "output_text", text: "custom-profile-answer" }],
  } });
  assert.match(run("luna", ["show", "--wave", "old.wave", "worker"]), /custom-profile-answer/);
  for (const wave of ["old.wave", "new"]) {
    json(join(state, "codex-swarm-events." + wave + ".run-" + wave + ".jsonl"), {
      name: "worker", method: "event-for-" + wave,
    });
  }
  const tail = run("luna", ["tail", "--wave", "old.wave", "worker"]);
  assert.match(tail, /event-for-old.wave/);
  assert.doesNotMatch(tail, /event-for-new/);

  // No app-server or model call is allowed: path validation must fail first.
  const forbidden = join(root, ".claude", "data");
  const linked = join(root, "linked");
  mkdirSync(forbidden, { recursive: true });
  symlinkSync(join(root, ".claude"), linked);
  for (const bad of [forbidden, join(linked, "missing")]) {
    for (const [name, args] of [
      ["luna", ["ls"]],
      ["codex-report", []], ["codex-steer", ["worker", "text"]],
      ["codex-watch", ["--wave", "test"]], ["codex-stop", ["--wave", "test"]],
      ["codex-daemon", ["start", "--wave", "test"]], ["codex-swarm.mjs", []],
    ]) {
      const output = run(name, args, { CODEX_AGENTS_STATE_DIR: bad, CODEX_BIN: "/no-such-codex" }, 1);
      assert.match(output, /outside .claude/);
    }
    assert.match(run("codex-models", [], { CODEX_HOME: bad, CODEX_BIN: "/no-such-codex" }, 1), /outside .claude/);
  }
  assert.deepEqual(readdirSync(forbidden), []);
  assert.equal(existsSync(join(linked, "missing")), false);
  passed = true;
  console.log("state contract smoke: PASS");
} finally {
  if (passed) rmSync(root, { recursive: true });
  else console.error("fixture retained: " + root);
}
