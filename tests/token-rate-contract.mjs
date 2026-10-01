import assert from "node:assert/strict";
import { test } from "node:test";
import {
  formatTokenRate,
  receiveTokenRate,
  subscribeTokenRate,
  tweenTokenRate,
  workerRateKey,
  receiveTeamTokenRates,
  clearTeamTokenRates,
} from "../web/src/tokenRate.ts";

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
test("volatile events stay scoped to each chat and reject malformed data", () => {
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
  assert.deepEqual(lead, [null, value]);
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
