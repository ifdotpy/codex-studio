import type { AgentEntityDto as Agent } from "../../src/generated/api.ts";
import type { BackgroundTask } from "../../src/types.ts";
import { snapshotFixture } from "../../src/snapshotFixture.ts";

/** The browser fixture's state identity and tasks extend the shared snapshot owner. */
export function fixtureSnapshot(agents: Agent[], tasks: BackgroundTask[] = []) {
  const snapshot = snapshotFixture(agents);
  return {
    ...snapshot,
    threads: agents,
    stateDir: "ux-chat-fixture",
    token: "fixture",
    runtime: { ...snapshot.runtime, agents, tasks, nativeNotices: [] },
  };
}
