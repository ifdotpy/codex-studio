import type { Agent, Snapshot } from "./types";

/** Minimal complete snapshot for unit witnesses; every collection uses its live owner. */
export function snapshotFixture(
  threads: Agent[] = [],
  runtime: Partial<Snapshot["runtime"]> = {},
): Snapshot {
  return {
    token: "",
    stateDir: "/fixture",
    threads,
    chats: [],
    nodes: threads,
    edges: [],
    runtime: {
      studioErrors: [],
      agents: threads,
      rooms: [],
      tasks: [],
      monitors: [],
      requests: [],
      complaints: [],
      rules: [],
      projects: [],
      peerTeams: [],
      events: [],
      work: [],
      ...runtime,
    },
  };
}
