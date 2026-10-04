import assert from "node:assert/strict";
import { it as test } from "vitest";
import { weeklyRunway } from "./weeklyRunway.ts";

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
  const signedOut = weeklyRunway(snapshot(20), now, true);
  assert.equal(signedOut.color, "gray");
  assert.equal(signedOut.windows[0].days, null);
  const stale = snapshot(20);
  stale.data.rateLimits.secondary.resetsAt = now - 1;
  assert.equal(weeklyRunway(stale, now).color, "green");
  assert.equal(weeklyRunway(stale, now).windows[0].remaining, 100);
  stale.data.rateLimits.secondary.usedPercent = null;
  assert.equal(weeklyRunway(stale, now).color, "green");
  const missingTime = snapshot(20);
  delete missingTime.at;
  assert.equal(weeklyRunway(missingTime, now).color, "red");
  assert.equal(weeklyRunway(missingTime, now).windows[0].days, null);
  const zeroWithoutTime = snapshot(0);
  delete zeroWithoutTime.at;
  assert.equal(weeklyRunway(zeroWithoutTime, now).color, "green");
  const zeroWithoutReset = snapshot(0);
  delete zeroWithoutReset.data.rateLimits.secondary.resetsAt;
  assert.equal(
    weeklyRunway(zeroWithoutReset, now).windows[0].resetFirst,
    false,
  );
  const missingWeekly = snapshot(20);
  delete missingWeekly.data.rateLimits.secondary;
  assert.equal(weeklyRunway(missingWeekly, now).color, "green");
  const exhausted = snapshot(100);
  delete exhausted.data.rateLimits.secondary.resetsAt;
  const noResetExhaustion = weeklyRunway(exhausted, now);
  assert.equal(noResetExhaustion.color, "red");
  assert.equal(noResetExhaustion.days, 0);
  assert.equal(noResetExhaustion.windows[0].resetFirst, false);
});

test("a cached Claude reset restores the full weekly allowance", () => {
  const limits = {
    accountKey: "claude-other",
    at: now - day,
    data: {
      rateLimitsByLimitId: {
        claude: {
          limitId: "claude",
          secondary: {
            usedPercent: 89,
            windowDurationMins: 10080,
            resetsAt: now - 10,
          },
        },
      },
    },
  };
  const result = weeklyRunway(limits, now);
  assert.equal(result.color, "green");
  assert.equal(result.windows[0].remaining, 100);
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

test("legacy and provider-specific limits preserve account and window labels", () => {
  const legacy = structuredClone(snapshot(20));
  delete legacy.data.rateLimits.limitId;
  const legacyResult = weeklyRunway(legacy, now);
  assert.equal(legacyResult.windows[0].name, "Codex");

  const emptyIndex = structuredClone(legacy);
  emptyIndex.data.rateLimitsByLimitId = {};
  assert.equal(weeklyRunway(emptyIndex, now).windows[0].name, "Codex");

  const indexed = {
    at: now,
    data: {
      rateLimitsByLimitId: {
        claude: {
          secondary: snapshot(20).data.rateLimits.secondary,
        },
        custom: {
          limitName: "Named pool",
          secondary: snapshot(20).data.rateLimits.secondary,
        },
      },
      rateLimits: snapshot(90).data.rateLimits,
    },
  };
  assert.deepEqual(
    weeklyRunway(indexed, now).windows.map(({ name }) => name),
    ["Claude", "Named pool"],
  );
});

test("only weekly windows with valid percentage and reset values contribute", () => {
  const limits = {
    at: now,
    data: {
      rateLimits: {
        primary: {
          usedPercent: 100,
          windowDurationMins: 300,
          resetsAt: now + 1,
        },
        secondary: {
          usedPercent: 20,
          windowDurationMins: 10080,
          resetsAt: now + 6 * day,
        },
      },
    },
  };
  const result = weeklyRunway(limits, now);
  assert.equal(result.windows.length, 1);
  assert.equal(result.windows[0].name, "Codex");
  assert.equal(result.windows[0].remaining, 80);

  for (const usedPercent of [-1, 101, "20", Infinity]) {
    const invalidUsage = structuredClone(limits);
    invalidUsage.data.rateLimits.secondary.usedPercent = usedPercent;
    const invalidResult = weeklyRunway(invalidUsage, now);
    assert.equal(invalidResult.windows[0].remaining, null);
    assert.equal(invalidResult.windows[0].days, null);
    assert.equal(invalidResult.color, "gray");
  }

  for (const resetsAt of [-1, "later", Infinity]) {
    const invalidReset = structuredClone(limits);
    invalidReset.data.rateLimits.secondary.resetsAt = resetsAt;
    const invalidResult = weeklyRunway(invalidReset, now);
    assert.equal(invalidResult.windows[0].reset, null);
    assert.equal(invalidResult.windows[0].days, null);
    assert.equal(invalidResult.color, "red");
    assert.equal(invalidResult.days, null);
    assert.equal(invalidResult.windows[0].resetFirst, false);
  }
  const zeroReset = structuredClone(limits);
  zeroReset.data.rateLimits.secondary.resetsAt = 0;
  assert.equal(weeklyRunway(zeroReset, now).windows[0].reset, null);

  const missingResetAtEpoch = structuredClone(limits);
  missingResetAtEpoch.at = 0;
  missingResetAtEpoch.data.rateLimits.secondary.resetsAt = undefined;
  const invalidAtEpoch = weeklyRunway(missingResetAtEpoch, 0);
  assert.equal(invalidAtEpoch.windows[0].days, null);
});

test("weekly reset and observation boundaries determine expiry and projection validity", () => {
  const atReset = snapshot(20);
  atReset.data.rateLimits.secondary.resetsAt = now;
  const expired = weeklyRunway(atReset, now);
  assert.equal(expired.windows[0].remaining, 100);
  assert.equal(expired.windows[0].days, Infinity);
  assert.equal(expired.color, "green");

  const nearFuture = snapshot(20);
  nearFuture.at = now + 5;
  assert.ok(
    Math.abs(weeklyRunway(nearFuture, now).windows[0].days - 4) < 0.001,
  );
  const tooFarFuture = snapshot(20);
  tooFarFuture.at = now + 6;
  assert.equal(weeklyRunway(tooFarFuture, now).windows[0].days, null);
  const beyondWindow = snapshot(20);
  beyondWindow.at = now + 5;
  beyondWindow.data.rateLimits.secondary.resetsAt = now + 1;
  assert.equal(weeklyRunway(beyondWindow, now).windows[0].days, null);

  const atWindowStart = snapshot(20, 0);
  assert.equal(weeklyRunway(atWindowStart, now).windows[0].days, null);
  const atWindowEnd = snapshot(20);
  atWindowEnd.at = now + 1;
  atWindowEnd.data.rateLimits.secondary.resetsAt = now + 1;
  assert.equal(weeklyRunway(atWindowEnd, now).windows[0].days, 28);

  const resetEqualsExhaustion = snapshot(50, 3.5);
  assert.equal(
    weeklyRunway(resetEqualsExhaustion, now).windows[0].resetFirst,
    false,
  );
  const resetMissing = snapshot(20);
  delete resetMissing.data.rateLimits.secondary.resetsAt;
  assert.equal(weeklyRunway(resetMissing, now).windows[0].resetFirst, false);
});

test("each weekly allowance reports its own remaining amount, projection and reset ordering", () => {
  const limits = {
    at: now,
    data: {
      rateLimitsByLimitId: {
        fast: {
          limitName: "Fast",
          secondary: snapshot(20, 1).data.rateLimits.secondary,
        },
        resetFirst: {
          limitName: "Reset first",
          secondary: snapshot(80, 6).data.rateLimits.secondary,
        },
        zero: {
          limitName: "Zero",
          secondary: snapshot(100, 1).data.rateLimits.secondary,
        },
      },
    },
  };
  const result = weeklyRunway(limits, now);
  assert.deepEqual(
    result.windows.map(({ name, remaining, days, resetFirst }) => ({
      name,
      remaining,
      days,
      resetFirst,
    })),
    [
      { name: "Fast", remaining: 80, days: 4, resetFirst: false },
      { name: "Reset first", remaining: 20, days: 1.5, resetFirst: true },
      { name: "Zero", remaining: 0, days: 0, resetFirst: false },
    ],
  );
  assert.equal(result.color, "red");
  assert.equal(result.days, 0);
  assert.equal(result.windows[2].resetFirst, false);

  const exhaustedAndUnknown = {
    at: now,
    data: {
      rateLimitsByLimitId: {
        exhausted: {
          secondary: snapshot(100).data.rateLimits.secondary,
        },
        unknown: {
          secondary: { windowDurationMins: 10080, usedPercent: 10 },
        },
      },
    },
  };
  const mixed = weeklyRunway(exhaustedAndUnknown, now);
  assert.equal(mixed.color, "red");
  assert.equal(mixed.days, 0);
});

test("empty and unknown weekly data return the states shown by the account allowance display", () => {
  assert.deepEqual(weeklyRunway({ data: {} }, now), {
    color: "green",
    days: Infinity,
    windows: [],
  });
  assert.equal(weeklyRunway({ error: "offline" }, now).color, "gray");

  const unknownWindows = {
    at: now,
    data: {
      rateLimitsByLimitId: {
        one: { secondary: { windowDurationMins: 10080, usedPercent: 110 } },
        two: null,
      },
    },
  };
  const unknown = weeklyRunway(unknownWindows, now);
  assert.equal(unknown.color, "gray");
  assert.deepEqual(
    unknown.windows.map(({ name, remaining, reset, days }) => ({
      name,
      remaining,
      reset,
      days,
    })),
    [{ name: "one", remaining: null, reset: null, days: null }],
  );

  const mixedKnownness = {
    at: now,
    data: {
      rateLimitsByLimitId: {
        unknown: { secondary: { windowDurationMins: 10080, usedPercent: 110 } },
        known: { secondary: snapshot(20).data.rateLimits.secondary },
      },
    },
  };
  const mixedResult = weeklyRunway(mixedKnownness, now);
  assert.equal(mixedResult.color, "red");
  assert.equal(mixedResult.days, null);

  const missingWeekly = snapshot(20);
  delete missingWeekly.data.rateLimits.secondary;
  assert.deepEqual(weeklyRunway(missingWeekly, now), {
    color: "green",
    days: Infinity,
    windows: [],
  });
});
