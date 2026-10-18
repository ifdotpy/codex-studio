import { expect, test } from "vitest";
import {
  canCheckConnection,
  matchingConnectionCheck,
} from "./ConnectionRecovery";
import {
  BACKEND_RESTARTED_TURN_ERROR,
  PROVIDER_DISCONNECTED_TURN_ERROR,
  VM_PROVIDER_DISCONNECTED_TURN_ERROR,
} from "../generated/sharedValues";
import type { Agent } from "../types";

const interrupted = (error: string, turnId: string | null) =>
  ({
    id: "agent-1",
    source: "managed",
    status: "interrupted",
    inFlight: false,
    autoWake: false,
    startAttempt: null,
    threadId: "thread-1",
    turnId,
    epoch: 4,
    accountKey: "default",
    error,
  }) as unknown as Agent;

test("a Linux VM provider disconnect offers Check connection", () => {
  expect(
    canCheckConnection(
      interrupted(VM_PROVIDER_DISCONNECTED_TURN_ERROR, "turn-1"),
    ),
  ).toBe(true);
  expect(
    canCheckConnection(interrupted(PROVIDER_DISCONNECTED_TURN_ERROR, "turn-1")),
  ).toBe(true);
});

test("a lost start with no turn ID offers Check connection after a transport loss only", () => {
  expect(
    canCheckConnection(interrupted(VM_PROVIDER_DISCONNECTED_TURN_ERROR, null)),
  ).toBe(true);
  expect(
    canCheckConnection(interrupted(PROVIDER_DISCONNECTED_TURN_ERROR, null)),
  ).toBe(true);
  expect(
    canCheckConnection(interrupted(BACKEND_RESTARTED_TURN_ERROR, null)),
  ).toBe(false);
  expect(canCheckConnection(interrupted("Another failure", "turn-1"))).toBe(
    false,
  );
});

test("a saved check without a turn ID matches the agent without a turn ID", () => {
  const agent = {
    ...interrupted(VM_PROVIDER_DISCONNECTED_TURN_ERROR, null),
    turnId: undefined,
    connectionCheck: {
      at: 10,
      epoch: 4,
      accountKey: "default",
      threadId: "thread-1",
      turnId: null,
      previousError: VM_PROVIDER_DISCONNECTED_TURN_ERROR,
      nativeState: "idle",
    },
  } as unknown as Agent;
  expect(matchingConnectionCheck(agent)).toBeTruthy();
});
