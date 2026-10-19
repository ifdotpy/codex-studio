import { expect, test } from "vitest";
import { startWaitLabel } from "./startWait";
import type { Agent } from "./types";

const queued = {
  id: "worker",
  status: "queued",
  epoch: 3,
  startWait: { reason: "slot", epoch: 3, runningTurns: 8, maxRunningTurns: 8 },
} as unknown as Agent;

test("a queued start shows the account slot or pause it waits for", () => {
  expect(startWaitLabel(queued)).toBe("Waiting for an account slot (8/8)");
  expect(
    startWaitLabel({
      ...queued,
      startWait: { ...queued.startWait!, reason: "pause", until: 10 },
    }),
  ).toBe("Waiting for the account pause");
});

test("a wait of another epoch or a started turn shows nothing", () => {
  expect(startWaitLabel({ ...queued, epoch: 4 })).toBeNull();
  expect(startWaitLabel({ ...queued, status: "running" })).toBeNull();
  expect(startWaitLabel({ ...queued, startWait: null })).toBeNull();
});
