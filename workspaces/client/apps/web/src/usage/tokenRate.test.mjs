import assert from "node:assert/strict";
import { it as test, vi } from "vitest";
import {
  createTokenRateCache,
  formatTokenRate,
  tweenTokenRate,
  workerRateKey,
} from "./tokenRate.ts";

test("tween and display formatting honor user-visible boundaries", () => {
  assert.equal(tweenTokenRate(1, 9, -1), 1);
  assert.equal(tweenTokenRate(1, 9, 451), 9);
  assert.ok(tweenTokenRate(10, 20, 225) > 10);
  assert.ok(tweenTokenRate(10, 20, 225) < 20);
  assert.equal(formatTokenRate(0), "0");
  assert.equal(formatTokenRate(0.99), "<1");
  assert.equal(formatTokenRate(1), "1");
  assert.equal(formatTokenRate(1.4), "1");
  assert.equal(formatTokenRate(1234), "1234");
  assert.equal(formatTokenRate(9999), "9999");
  assert.equal(
    formatTokenRate(10000),
    new Intl.NumberFormat(undefined, {
      notation: "compact",
      maximumFractionDigits: 0,
      useGrouping: false,
    }).format(10000),
  );
  const runtimeLocaleCompact = new Intl.NumberFormat(undefined, {
    notation: "compact",
    maximumFractionDigits: 0,
    useGrouping: false,
  }).format(1e16);
  assert.equal(formatTokenRate(1e16), runtimeLocaleCompact);
});

test("team and footer interests share snapshots and replay on reopen", () => {
  const cache = createTokenRateCache();
  const callbacks = new Set();
  vi.stubGlobal("window", {});
  cache.configureTokenRateStream((listener) => {
    callbacks.add(listener);
    return () => callbacks.delete(listener);
  });
  const record = {
    agentId: "worker",
    rootId: "team",
    turnId: "turn",
    active: true,
    estimated: false,
    rate: 42,
    outputTokens: 84,
  };
  const footer = [],
    worker = [];
  const stopFooter = cache.subscribeTokenRate("worker", (value) =>
    footer.push(value),
  );
  const stopWorker = cache.subscribeTokenRate(
    workerRateKey("team", "worker"),
    (value) => worker.push(value),
  );
  const closeFooter = cache.watchTokenRateInterest();
  let closeTeam = cache.watchTeamTokenRates("team");
  try {
    assert.equal(callbacks.size, 2);
    const publish = (record) => {
      const event = {
        protocol: 3,
        workspaceId: "workspace",
        epoch: "epoch",
        revision: 1,
        records: [record],
      };
      assert.equal(cache.receiveResourceTokenRates(event), true);
      for (const callback of callbacks) callback(event);
    };
    publish(record);
    assert.deepEqual(footer, [null, record]);
    assert.deepEqual(worker, [null, record]);
    closeTeam();
    closeTeam = undefined;
    const latest = { ...record, active: false, rate: 64 };
    publish(latest);
    assert.equal(callbacks.size, 1);
    assert.deepEqual(footer, [null, record, latest]);
    assert.equal(worker.at(-1), null);
    closeTeam = cache.watchTeamTokenRates("team");
    assert.deepEqual(worker.at(-1), latest);
    assert.equal(callbacks.size, 2);
  } finally {
    closeTeam?.();
    closeFooter();
    stopFooter();
    stopWorker();
    vi.unstubAllGlobals();
  }
});

test("subscriptions unsubscribe independently and retain completed turn samples", () => {
  const cache = createTokenRateCache();
  const first = [],
    second = [];
  const stopFirst = cache.subscribeTokenRate("agent", (value) =>
    first.push(value),
  );
  const stopSecond = cache.subscribeTokenRate("agent", (value) =>
    second.push(value),
  );
  const record = {
    agentId: "agent",
    rootId: "root",
    turnId: "finished",
    active: false,
    estimated: false,
    rate: 4,
    outputTokens: 8,
  };
  cache.receiveWorkspaceTokenRates({ records: [record] });
  stopFirst();
  cache.clearWorkspaceTokenRates();
  assert.deepEqual(first, [null, record]);
  assert.deepEqual(second, [null, record, null]);
  assert.equal(cache.getWorkspaceTeamTokenRate("root", "agent"), null);
  stopSecond();
});
