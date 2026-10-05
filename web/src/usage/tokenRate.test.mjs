import assert from "node:assert/strict";
import { it as test } from "vitest";
import {
  formatTokenRate,
  getTokenRate,
  getWorkspaceTeamTokenRate,
  receiveTokenRate,
  subscribeTokenRate,
  tweenTokenRate,
  workerRateKey,
  receiveTeamTokenRates,
  clearTeamTokenRates,
  watchTeamTokenRates,
  receiveWorkspaceTokenRates,
  clearWorkspaceTokenRates,
} from "./tokenRate.ts";

test("rate tween has a bounded duration and no overshoot", () => {
  assert.equal(tweenTokenRate(20, 80, 0), 20);
  assert.ok(tweenTokenRate(20, 80, 100) > 20);
  assert.ok(tweenTokenRate(20, 80, 100) < 80);
  assert.equal(tweenTokenRate(20, 80, 450), 80);
  assert.equal(tweenTokenRate(80, 20, 900), 20);
});
test("format preserves low rates and bounds large values", () => {
  assert.equal(formatTokenRate(0.01), "<1");
  assert.equal(formatTokenRate(42), "42");
  assert.ok(formatTokenRate(123456).length <= 5);
});
test("volatile events stay scoped to each chat after stream compatibility is checked", () => {
  const lead = [],
    worker = [];
  const stopLead = subscribeTokenRate("lead", (value) => lead.push(value));
  const stopWorker = subscribeTokenRate("worker", (value) =>
    worker.push(value),
  );
  const value = {
    turnId: "one",
    active: true,
    estimated: true,
    rate: 42,
    outputTokens: 84,
  };
  receiveTokenRate("lead", { data: JSON.stringify(value) });
  receiveTokenRate("lead", { data: JSON.stringify({ ...value, rate: -1 }) });
  assert.deepEqual(lead, [null, value, { ...value, rate: -1 }]);
  assert.deepEqual(worker, [null]);
  stopLead();
  stopWorker();
});

test("team batches isolate teams and footers and clear absent workers", () => {
  const worker = [],
    otherTeam = [],
    footer = [];
  const stop = subscribeTokenRate(workerRateKey("team", "worker"), (value) =>
    worker.push(value),
  );
  const stopOther = subscribeTokenRate(
    workerRateKey("other", "worker"),
    (value) => otherTeam.push(value),
  );
  const stopFooter = subscribeTokenRate("worker", (value) =>
    footer.push(value),
  );
  const value = {
    turnId: "turn",
    active: true,
    estimated: true,
    rate: 42,
    outputTokens: 84,
  };
  receiveTeamTokenRates("team", {
    data: JSON.stringify({ teamId: "other", rates: { worker: value } }),
  });
  receiveTeamTokenRates("team", {
    data: JSON.stringify({ teamId: "team", rates: { worker: value } }),
  });
  assert.deepEqual(worker, [null, value]);
  assert.deepEqual(otherTeam, [null]);
  assert.deepEqual(footer, [null]);
  receiveTeamTokenRates("team", {
    data: JSON.stringify({ teamId: "team", rates: {} }),
  });
  assert.deepEqual(worker, [null, value, null]);
  clearTeamTokenRates("team");
  stop();
  stopOther();
  stopFooter();
});

test("shared workspace batches reach open teams and footers without sync writes", () => {
  const worker = [],
    footer = [];
  const stopWorker = subscribeTokenRate(
    workerRateKey("shared-team", "shared-worker"),
    (rate) => worker.push(rate),
  );
  const stopFooter = subscribeTokenRate("shared-worker", (rate) =>
    footer.push(rate),
  );
  const value = {
    turnId: "one",
    active: true,
    estimated: true,
    rate: 42,
    outputTokens: 84,
  };
  const batch = {
    rates: { "shared-worker": value },
    teams: { "shared-team": { "shared-worker": value } },
  };
  assert.equal(receiveWorkspaceTokenRates(batch), true);
  assert.deepEqual(worker, [null], "a closed Team receives no card updates");
  assert.deepEqual(footer, [null, value]);
  const closeTeam = watchTeamTokenRates("shared-team");
  assert.deepEqual(
    worker,
    [null, value],
    "opening Team reads the latest shared batch",
  );
  const before = footer.length;
  assert.equal(
    receiveWorkspaceTokenRates({
      ...batch,
      rates: { "shared-worker": { ...value, rate: -1 } },
    }),
    true,
  );
  assert.equal(footer.length, before + 1);
  closeTeam();
  assert.equal(worker.at(-1), null);
  receiveWorkspaceTokenRates(batch);
  assert.equal(worker.at(-1), null, "closing Team stops card updates");
  clearWorkspaceTokenRates();
  assert.equal(footer.at(-1), null);
  stopWorker();
  stopFooter();
});

test("a newly opened team reads the current inactive worker sample immediately", () => {
  const agent = "remembered-worker";
  const key = workerRateKey("remembered-team", agent);
  const value = {
    turnId: "finished-turn",
    active: false,
    estimated: false,
    rate: 64,
    outputTokens: 256,
  };
  assert.equal(
    receiveWorkspaceTokenRates({
      rates: { [agent]: value },
      teams: { "remembered-team": { [agent]: value } },
    }),
    true,
  );
  assert.deepEqual(getTokenRate(agent), value);
  assert.deepEqual(getWorkspaceTeamTokenRate("remembered-team", agent), value);
  const close = watchTeamTokenRates("remembered-team");
  assert.deepEqual(getTokenRate(key), value);
  close();
});

test("chat samples accept stream values without a duplicate field-shape walk", () => {
  const events = [];
  const stop = subscribeTokenRate("validation-chat", (value) =>
    events.push(value),
  );
  const valid = {
    turnId: "turn",
    active: true,
    estimated: false,
    rate: 0,
    outputTokens: 0,
  };
  receiveTokenRate("validation-chat", { data: JSON.stringify(valid) });
  for (const invalid of [
    { ...valid, turnId: 1 },
    { ...valid, active: 1 },
    { ...valid, estimated: 1 },
    { ...valid, rate: -1 },
    { ...valid, rate: Infinity },
    { ...valid, outputTokens: -1 },
    { ...valid, outputTokens: NaN },
  ])
    receiveTokenRate("validation-chat", { data: JSON.stringify(invalid) });
  receiveTokenRate("validation-chat", { data: "{" });
  assert.deepEqual(getTokenRate("validation-chat"), {
    ...valid,
    outputTokens: null,
  });
  receiveTokenRate("validation-chat", { data: "null" });
  assert.equal(getTokenRate("validation-chat"), null);
  assert.deepEqual(events.at(-1), null);
  stop();
});

test("publishing token rates refreshes their LRU position and evicts old samples", () => {
  const samples = Array.from({ length: 2050 }, (_, index) => ({
    turnId: String(index),
    active: false,
    estimated: false,
    rate: index,
    outputTokens: index,
  }));
  for (let index = 0; index < samples.length; index++)
    receiveTokenRate(`cache-sample-${index}`, {
      data: JSON.stringify(samples[index]),
    });
  assert.equal(getTokenRate("cache-sample-0"), null);
  assert.deepEqual(getTokenRate("cache-sample-2"), samples[2]);
  assert.deepEqual(getTokenRate("cache-sample-2049"), samples[2049]);
});

test("an updated sample moves to the newest cache position", () => {
  const first = {
    turnId: "first",
    active: false,
    estimated: false,
    rate: 1,
    outputTokens: 1,
  };
  receiveTokenRate("lru-first", { data: JSON.stringify(first) });
  for (let index = 0; index < 2047; index++)
    receiveTokenRate(`lru-fill-${index}`, { data: JSON.stringify(first) });
  const refreshed = { ...first, turnId: "refreshed", rate: 2 };
  receiveTokenRate("lru-first", { data: JSON.stringify(refreshed) });
  receiveTokenRate("lru-newest", { data: JSON.stringify(first) });
  assert.deepEqual(getTokenRate("lru-first"), refreshed);
  assert.deepEqual(getTokenRate("lru-newest"), first);
});

test("team events retain container and key limits without walking each rate shape", () => {
  const key = workerRateKey("guarded-team", "worker");
  const values = [];
  const stop = subscribeTokenRate(key, (value) => values.push(value));
  const valid = {
    turnId: "turn",
    active: true,
    estimated: true,
    rate: 3,
    outputTokens: 6,
  };
  const send = (data) =>
    receiveTeamTokenRates("guarded-team", { data: JSON.stringify(data) });
  send({ teamId: "guarded-team", rates: { worker: valid } });
  for (const batch of [
    null,
    { teamId: "guarded-team", rates: [] },
    { teamId: "guarded-team", rates: null },
    { teamId: "guarded-team", rates: 42 },
    { teamId: "guarded-team", rates: { ["x".repeat(201)]: valid } },
    {
      teamId: "guarded-team",
      rates: Object.fromEntries(
        Array.from({ length: 1025 }, (_, index) => [`worker-${index}`, valid]),
      ),
    },
  ])
    send(batch);
  receiveTeamTokenRates("guarded-team", { data: "not-json" });
  assert.deepEqual(getTokenRate(key), valid);
  assert.deepEqual(values, [null, valid]);
  send({ teamId: "guarded-team", rates: { worker: { ...valid, rate: -2 } } });
  assert.equal(getTokenRate(key).rate, -2);
  send({ teamId: "guarded-team", rates: { worker: null } });
  assert.equal(getTokenRate(key), null);
  assert.deepEqual(values, [null, valid, { ...valid, rate: -2 }, null]);
  clearTeamTokenRates("guarded-team");
  const afterClear = values.length;
  clearTeamTokenRates("guarded-team");
  assert.equal(values.length, afterClear);
  stop();
});

test("team batches accept exact member-count and identifier limits", () => {
  const values = [];
  const id = "i".repeat(200);
  const key = workerRateKey("bounded-team", id);
  const stop = subscribeTokenRate(key, (value) => values.push(value));
  const valid = {
    turnId: "turn",
    active: false,
    estimated: false,
    rate: 1,
    outputTokens: 1,
  };
  const rates = Object.fromEntries(
    Array.from({ length: 1023 }, (_, index) => [`member-${index}`, valid]),
  );
  rates[id] = valid;
  receiveTeamTokenRates("bounded-team", {
    data: JSON.stringify({ teamId: "bounded-team", rates }),
  });
  assert.deepEqual(getTokenRate(key), valid);
  assert.deepEqual(values, [null, valid]);
  stop();
});

test("clearing a team removes its inactive cache entry", () => {
  const sample = {
    turnId: "cached",
    active: false,
    estimated: false,
    rate: 1,
    outputTokens: 1,
  };
  for (let index = 0; index < 2048; index++)
    receiveTokenRate(`clear-cache-fill-${index}`, {
      data: JSON.stringify(sample),
    });
  receiveTokenRate("clear-cache-sentinel", { data: JSON.stringify(sample) });
  const team = "clear-cache-team";
  const key = workerRateKey(team, "worker");
  receiveTeamTokenRates(team, {
    data: JSON.stringify({ teamId: team, rates: { worker: sample } }),
  });
  clearTeamTokenRates(team);
  for (let index = 0; index < 2047; index++)
    receiveTokenRate(`clear-cache-after-${index}`, {
      data: JSON.stringify(sample),
    });
  assert.deepEqual(getTokenRate("clear-cache-sentinel"), sample);
  assert.equal(getTokenRate(key), null);
});

test("workspace snapshots validate shape and limits atomically", () => {
  const key = "workspace-guard-worker";
  const values = [];
  const stop = subscribeTokenRate(key, (value) => values.push(value));
  const valid = {
    turnId: "turn",
    active: false,
    estimated: false,
    rate: 8,
    outputTokens: 16,
  };
  assert.equal(
    receiveWorkspaceTokenRates({ rates: { [key]: valid }, teams: {} }),
    true,
  );
  const updated = { ...valid, rate: 9 };
  assert.equal(
    receiveWorkspaceTokenRates({ rates: { [key]: updated }, teams: {} }),
    true,
  );
  assert.deepEqual(values, [null, valid, updated]);
  for (const invalid of [
    null,
    undefined,
    { rates: null, teams: {} },
    { rates: {}, teams: null },
    { rates: {}, teams: 42 },
    { rates: {}, teams: [] },
    { rates: 42, teams: {} },
    { rates: {}, teams: { ["x".repeat(201)]: {} } },
    { rates: { ["x".repeat(201)]: valid }, teams: {} },
    { rates: {}, teams: { valid: { ["x".repeat(201)]: valid } } },
    {
      rates: {},
      teams: Object.fromEntries(
        Array.from({ length: 1025 }, (_, index) => [`team-${index}`, {}]),
      ),
    },
    {
      rates: {},
      teams: {
        oversized: Object.fromEntries(
          Array.from({ length: 1025 }, (_, index) => [
            `worker-${index}`,
            valid,
          ]),
        ),
      },
    },
  ])
    assert.equal(receiveWorkspaceTokenRates(invalid), false);
  assert.deepEqual(getTokenRate(key), updated);
  assert.deepEqual(values, [null, valid, updated]);
  assert.equal(getWorkspaceTeamTokenRate("missing-team", "worker"), null);
  assert.equal(
    receiveWorkspaceTokenRates({
      rates: { ["x".repeat(200)]: valid },
      teams: { ["t".repeat(200)]: { ["a".repeat(200)]: valid } },
    }),
    true,
  );
  assert.equal(
    receiveWorkspaceTokenRates({
      rates: Object.fromEntries(
        Array.from({ length: 1024 }, (_, index) => [`rate-${index}`, valid]),
      ),
      teams: {},
    }),
    true,
  );
  assert.equal(
    receiveWorkspaceTokenRates({
      rates: {},
      teams: Object.fromEntries(
        Array.from({ length: 1024 }, (_, index) => [`team-${index}`, {}]),
      ),
    }),
    true,
  );
  clearWorkspaceTokenRates();
  assert.equal(getTokenRate(key), null);
  assert.deepEqual(values, [null, valid, updated, null]);
  stop();
});

test("team watchers stay active until the last subscriber closes", () => {
  const team = "shared-watchers";
  const key = workerRateKey(team, "worker");
  const values = [];
  const stop = subscribeTokenRate(key, (value) => values.push(value));
  const first = watchTeamTokenRates(team);
  const second = watchTeamTokenRates(team);
  const value = {
    turnId: "active",
    active: true,
    estimated: true,
    rate: 12,
    outputTokens: 24,
  };
  assert.equal(
    receiveWorkspaceTokenRates({
      rates: {},
      teams: { [team]: { worker: value } },
    }),
    true,
  );
  assert.deepEqual(values, [null, value]);
  first();
  assert.equal(
    receiveWorkspaceTokenRates({
      rates: {},
      teams: { [team]: { worker: { ...value, rate: 13 } } },
    }),
    true,
  );
  assert.deepEqual(values, [null, value, { ...value, rate: 13 }]);
  const beforeLastClose = values.length;
  second();
  assert.equal(values.length, beforeLastClose + 1);
  assert.equal(values.at(-1), null);
  assert.equal(getTokenRate(key), null);
  receiveWorkspaceTokenRates({
    rates: {},
    teams: { [team]: { worker: value } },
  });
  assert.equal(getTokenRate(key), null);
  const reopened = watchTeamTokenRates(team);
  assert.deepEqual(getTokenRate(key), value);
  reopened();
  stop();
});

test("subscriptions fan out once and unsubscribe independently", () => {
  const firstEvents = [];
  const secondEvents = [];
  const listener = (value) => firstEvents.push(value);
  const stopFirst = subscribeTokenRate("two-listeners", listener);
  const stopSecond = subscribeTokenRate("two-listeners", (value) =>
    secondEvents.push(value),
  );
  const value = {
    turnId: "turn",
    active: true,
    estimated: false,
    rate: 4,
    outputTokens: 8,
  };
  receiveTokenRate("two-listeners", { data: JSON.stringify(value) });
  stopFirst();
  receiveTokenRate("two-listeners", { data: "null" });
  assert.deepEqual(firstEvents, [null, value]);
  assert.deepEqual(secondEvents, [null, value, null]);
  stopSecond();
});

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
