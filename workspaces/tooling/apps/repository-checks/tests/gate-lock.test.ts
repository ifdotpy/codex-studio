import test from "node:test";
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { withGateLock } from "../gate-lock.ts";

const fresh = () =>
  path.join(mkdtempSync(path.join(tmpdir(), "gate-lock-")), "lock");
const hold = (lock: string, pid: number) => {
  mkdirSync(lock);
  writeFileSync(path.join(lock, "pid"), `${pid}\n`);
};
const fast = { pollMs: 10, noticeMs: 30 };

test("a second runner waits while a live pid holds the lock, then runs", async () => {
  const lock = fresh();
  const holder = spawn("sleep", ["60"]);
  hold(lock, holder.pid!);
  const lines: string[] = [];
  let ran = false;
  const waiting = withGateLock(
    async () => {
      ran = true;
      assert.equal(
        readFileSync(path.join(lock, "pid"), "utf8").trim(),
        String(process.pid),
      );
    },
    { lockPath: lock, ...fast, write: (text) => lines.push(text) },
  );
  await new Promise((resolve) => setTimeout(resolve, 150));
  assert.equal(ran, false);
  assert.match(lines[0] ?? "", new RegExp(`pid ${holder.pid}\\b.*sleep`));
  holder.kill();
  await waiting;
  assert.equal(ran, true);
  assert.equal(existsSync(lock), false);
});

test("a wait limit fails with the holder pid", async () => {
  const lock = fresh();
  const holder = spawn("sleep", ["60"]);
  hold(lock, holder.pid!);
  try {
    await assert.rejects(
      withGateLock(async () => {}, { lockPath: lock, ...fast, maxWaitMs: 50 }),
      new RegExp(`held by pid ${holder.pid}`),
    );
  } finally {
    holder.kill();
  }
});

test("a lock held by a dead pid is taken over", async () => {
  const lock = fresh();
  const dead = spawnSync("true");
  hold(lock, dead.pid!);
  const ran = await withGateLock(async () => true, { lockPath: lock, ...fast });
  assert.equal(ran, true);
  assert.equal(existsSync(lock), false);
});

test("a lock held by an ancestor does not block and stays held", async () => {
  const lock = fresh();
  hold(lock, process.ppid);
  const ran = await withGateLock(async () => true, { lockPath: lock, ...fast });
  assert.equal(ran, true);
  assert.equal(
    readFileSync(path.join(lock, "pid"), "utf8").trim(),
    String(process.ppid),
  );
});

test("the lock is released after an error", async () => {
  const lock = fresh();
  await assert.rejects(
    withGateLock(
      async () => {
        throw new Error("boom");
      },
      { lockPath: lock, ...fast },
    ),
    /boom/,
  );
  assert.equal(existsSync(lock), false);
});
