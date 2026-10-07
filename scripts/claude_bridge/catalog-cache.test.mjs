import assert from "node:assert/strict";
import { test } from "vitest";
import { createCatalogCache } from "./catalog-cache.mjs";

const settle = async () => {
  for (let i = 0; i < 8; i++) await Promise.resolve();
};

function fixture() {
  let at = 0;
  let active = true;
  const timers = new Set();
  const calls = [];
  const cache = createCatalogCache({
    now: () => at,
    isActive: () => active,
    setTimer(fn, delay) {
      const timer = {
        fn,
        due: at + delay,
        unref() {
          this.unreferenced = true;
        },
      };
      timers.add(timer);
      return timer;
    },
    clearTimer(timer) {
      timers.delete(timer);
    },
    load: () =>
      new Promise((resolve, reject) => calls.push({ resolve, reject, at })),
  });
  return {
    cache,
    calls,
    timers,
    move(value) {
      at = value;
      for (const timer of timers) {
        if (timer.due <= at) {
          timers.delete(timer);
          timer.fn();
        }
      }
    },
    activity(value) {
      active = value;
      cache.activityChanged();
    },
    async prime(value = { models: ["original"], account: "original" }) {
      const read = cache.read();
      await settle();
      calls[0].resolve(value);
      assert.equal(await read, value);
      await settle();
      return value;
    },
  };
}

test("foreground metadata requests share one exact probe", async () => {
  const f = fixture();
  const a = f.cache.read();
  const b = f.cache.read();
  assert.equal(a, b);
  await settle();
  assert.equal(f.calls.length, 1);
  f.calls[0].resolve({ models: [] });
  assert.equal(await a, await b);
  f.cache.close();
});

test("background refresh serves only unexpired proof and joins after expiry", async () => {
  const f = fixture();
  const old = await f.prime();
  assert.equal(f.timers.size, 1);
  assert.equal([...f.timers][0].due, 240000);
  assert.equal([...f.timers][0].unreferenced, true);
  f.move(240000);
  await settle();
  assert.equal(f.calls.length, 2);
  assert.equal(await f.cache.read(), old);
  f.move(300000);
  const expired = f.cache.read();
  assert.equal(expired, f.cache.read());
  let finished = false;
  void expired.then(() => {
    finished = true;
  });
  await settle();
  assert.equal(finished, false);
  const next = { models: ["new"], account: "original" };
  f.calls[1].resolve(next);
  assert.equal(await expired, next);
  assert.equal(f.calls.length, 2);
  f.cache.close();
});

test("failed background refresh never extends TTL or enters a retry loop", async () => {
  const f = fixture();
  const old = await f.prime();
  f.move(240000);
  await settle();
  f.calls[1].reject(new Error("native unavailable"));
  await settle();
  assert.equal(await f.cache.read(), old);
  f.activity(true);
  assert.equal(f.timers.size, 0);
  f.move(299999);
  assert.equal(await f.cache.read(), old);
  assert.equal(f.calls.length, 2);
  f.move(300000);
  const expired = f.cache.read();
  const rejection = assert.rejects(expired, /native unavailable/);
  await settle();
  assert.equal(f.calls.length, 3);
  f.calls[2].reject(new Error("native unavailable"));
  await rejection;
  f.cache.close();
});

test("proven account rejection invalidates an unexpired proof", async () => {
  const f = fixture();
  const old = await f.prime();
  f.move(240000);
  await settle();
  f.calls[1].reject(
    Object.assign(new Error("account changed"), {
      claudeAccountValidationFailed: true,
    }),
  );
  await settle();
  const read = f.cache.read();
  const rejection = assert.rejects(read, /account changed/);
  await settle();
  assert.equal(f.calls.length, 3);
  f.calls[2].reject(new Error("account changed"));
  await rejection;
  assert.equal(old.account, "original");
  f.cache.close();
});

test("idle or closed queries cancel refresh and resume only on active work", async () => {
  const f = fixture();
  await f.prime();
  const oldTimer = [...f.timers][0];
  f.activity(false);
  assert.equal(f.timers.size, 0);
  f.move(250000);
  oldTimer.fn();
  await settle();
  assert.equal(f.calls.length, 1);
  f.activity(true);
  assert.equal(f.timers.size, 1);
  oldTimer.fn();
  await settle();
  assert.equal(f.calls.length, 1);
  assert.equal(f.timers.size, 1);
  f.move(250000);
  await settle();
  assert.equal(f.calls.length, 2);
  f.cache.close();
  f.calls[1].resolve({ models: ["late"] });
  await settle();
  assert.equal(f.timers.size, 0);
  await assert.rejects(f.cache.read(), /cache closed/);
});

test("each successful proof permits one next background refresh", async () => {
  const f = fixture();
  await f.prime();
  f.move(240000);
  await settle();
  f.calls[1].resolve({ models: ["next"] });
  await settle();
  assert.equal([...f.timers][0].due, 480000);
  f.activity(true);
  f.activity(true);
  assert.equal(f.timers.size, 1);
  f.move(480000);
  await settle();
  assert.equal(f.calls.length, 3);
  f.cache.close();
  f.calls[2].resolve({ models: ["last"] });
  await settle();
});

test("proof freshness starts at request time, not slow completion time", async () => {
  const f = fixture();
  const read = f.cache.read();
  await settle();
  f.move(310000);
  f.calls[0].resolve({ models: ["old"] });
  await read;
  await settle();
  assert.equal(f.timers.size, 0);
  const next = f.cache.read();
  await settle();
  assert.equal(f.calls.length, 2);
  f.calls[1].resolve({ models: ["new"] });
  await next;
  f.cache.close();
});

test("different bridge accounts do not share cached proof", async () => {
  const a = fixture();
  const b = fixture();
  await a.prime({ account: "a" });
  await b.prime({ account: "b" });
  assert.equal((await a.cache.read()).account, "a");
  assert.equal((await b.cache.read()).account, "b");
  a.cache.close();
  b.cache.close();
});
