import { expect, test } from "vitest";
import { StartWaitReason } from "./generated/api";
import { startWaitLabel } from "./startWait";
import type { Agent } from "./types";
import type { components } from "./generated/api";

type Wait = components["schemas"]["AgentEntityStartWaitDto"];
type ExpectFalse<T extends false> = T;
type MissingSlotEvidence = ExpectFalse<
  { reason: typeof StartWaitReason.slot; epoch: number } extends Wait
    ? true
    : false
>;
type MissingPauseEvidence = ExpectFalse<
  { reason: typeof StartWaitReason.pause; epoch: number } extends Wait
    ? true
    : false
>;
const missingEvidenceWitness: [MissingSlotEvidence, MissingPauseEvidence] = [
  false,
  false,
];

const queued = {
  id: "worker",
  status: "queued",
  epoch: 3,
  startWait: {
    reason: StartWaitReason.slot,
    epoch: 3,
    runningTurns: 8,
    maxRunningTurns: 8,
  },
} as unknown as Agent;

test("a queued start shows the account slot or pause it waits for", () => {
  expect(missingEvidenceWitness).toEqual([false, false]);
  expect(startWaitLabel(queued)).toBe("Waiting for an account slot (8/8)");
  expect(
    startWaitLabel({
      ...queued,
      startWait: { reason: StartWaitReason.pause, epoch: 3, until: 10 },
    }),
  ).toBe("Waiting for the account pause");
});

test("a wait of another epoch or a started turn shows nothing", () => {
  expect(startWaitLabel({ ...queued, epoch: 4 })).toBeNull();
  expect(startWaitLabel({ ...queued, status: "running" })).toBeNull();
  expect(startWaitLabel({ ...queued, startWait: null })).toBeNull();
});

test("the owner reason remains visible without invented account gate details", () => {
  for (const [reason, label] of [
    [StartWaitReason.disk, "Waiting for free disk space"],
    [StartWaitReason.model_catalog, "Waiting for the model catalog"],
    [StartWaitReason.provider_session, "Waiting for the provider session"],
    [StartWaitReason.dependency, "Waiting for a preparation dependency"],
  ] as const) {
    expect(startWaitLabel({ ...queued, startWait: { reason, epoch: 3 } })).toBe(
      label,
    );
  }
});
