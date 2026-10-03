import assert from "node:assert/strict";
import { test } from "node:test";
import { weeklyRunway } from "../web/src/weeklyRunway.ts";

const now = 1800000000;
const day = 86400;
const snapshot = (usedPercent, elapsed = 1) => ({
  at: now,
  data: {
    rateLimits: {
      limitId: "codex",
      primary: {
        usedPercent: 100,
        windowDurationMins: 300,
        resetsAt: now + 3600,
      },
      secondary: {
        usedPercent,
        windowDurationMins: 10080,
        resetsAt: now + (7 - elapsed) * day,
      },
    },
  },
});

test("weekly consumption controls all three bands; the exhausted short window is ignored", () => {
  assert.equal(weeklyRunway(snapshot(10), now).color, "green");
  const yellow = weeklyRunway(snapshot(15), now);
  assert.equal(yellow.color, "yellow");
  assert.ok(Math.abs(yellow.days - 85 / 15) < 1e-9);
  const red = weeklyRunway(snapshot(20), now);
  assert.equal(red.color, "red");
  assert.equal(red.days, 4);
  assert.equal(weeklyRunway(snapshot(100), now).color, "red");
});

test("reset before exhaustion is green even with fewer than five projected days", () => {
  const result = weeklyRunway(snapshot(80, 6), now);
  assert.equal(result.windows[0].days, 1.5);
  assert.equal(result.windows[0].resetFirst, true);
  assert.equal(result.color, "green");
  assert.equal(weeklyRunway(snapshot(0), now).color, "green");
});

test("five days starts the yellow band and seven days is green", () => {
  const yellow = weeklyRunway(snapshot(20, 1.25), now);
  assert.equal(yellow.days, 5);
  assert.equal(yellow.color, "yellow");
  assert.equal(weeklyRunway(snapshot(20, 1.249), now).color, "red");
  const green = weeklyRunway(snapshot(20, 1.75), now);
  assert.equal(green.days, 7);
  assert.equal(green.color, "green");
});

test("unknown observations and signed-out accounts stay gray", () => {
  assert.equal(weeklyRunway(null, now).color, "gray");
  assert.equal(weeklyRunway(snapshot(null), now).color, "gray");
  assert.equal(weeklyRunway(snapshot(20), now, true).color, "gray");
  const stale = snapshot(20);
  stale.data.rateLimits.secondary.resetsAt = now - 1;
  assert.equal(weeklyRunway(stale, now).color, "gray");
  const missingTime = snapshot(20);
  delete missingTime.at;
  assert.equal(weeklyRunway(missingTime, now).color, "gray");
  const missingWeekly = snapshot(20);
  delete missingWeekly.data.rateLimits.secondary;
  assert.equal(weeklyRunway(missingWeekly, now).color, "gray");
  const exhausted = snapshot(100);
  delete exhausted.data.rateLimits.secondary.resetsAt;
  assert.equal(weeklyRunway(exhausted, now).color, "red");
});

test("the least sustainable weekly pool controls a Claude account", () => {
  const limits = {
    at: now,
    data: {
      rateLimitsByLimitId: {
        claude: {
          limitName: "Claude",
          secondary: snapshot(80, 6).data.rateLimits.secondary,
        },
        "claude-sonnet": {
          limitName: "Claude Sonnet",
          secondary: snapshot(20).data.rateLimits.secondary,
        },
      },
    },
  };
  assert.equal(weeklyRunway(limits, now).color, "red");
  limits.data.rateLimitsByLimitId["claude-sonnet"].secondary =
    snapshot(15).data.rateLimits.secondary;
  assert.equal(weeklyRunway(limits, now).color, "yellow");
});

test("the consumption rate uses the observation time, not time since a stale snapshot", () => {
  const limits = snapshot(20);
  assert.equal(weeklyRunway(limits, now + day).windows[0].days, 4);
});
