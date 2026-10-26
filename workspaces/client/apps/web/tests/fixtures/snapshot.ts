import type { Agent, BackgroundTask, Snapshot } from "../../src/types.ts";

/** A complete renderer snapshot for isolated component fixtures. */
export function fixtureSnapshot(agents: Agent[], tasks: BackgroundTask[] = []) {
  return {
    stateDir: "ux-chat-fixture",
    token: "fixture",
    threads: agents,
    nodes: agents,
    chats: [],
    edges: [],
    runtime: {
      agents,
      tasks,
      rooms: [],
      monitors: [],
      complaints: [],
      requests: [],
      rules: [],
      projects: [],
      peerTeams: [],
      events: [],
      work: [],
      studioErrors: [],
      nativeNotices: [],
    },
  } satisfies Snapshot;
}
