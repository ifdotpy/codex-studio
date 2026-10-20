import { expect, it } from "vitest";
import { roomLeadIds } from "./chatScope";
import type { Agent, Room } from "./types";

const TEAM_SIZE = 512;
const ROOM_COUNT = 256;
const MEMBERS_PER_ROOM = 4;

// Reference: the scan of all agents that roomLeadIds replaced.
function scanLeadIds(room: Room, agents: Agent[]) {
  const participants = new Set(room.members);
  return [
    ...new Set(
      agents
        .filter((agent) => participants.has(agent.id))
        .map((agent) => (agent.isLead ? agent.id : agent.rootId))
        .filter((id): id is string => !!id),
    ),
  ];
}

it("resolves room leads by member without scanning the whole team", () => {
  let reads = 0;
  const agents = Array.from({ length: TEAM_SIZE }, (_, index) => {
    const agent = {
      id: `a${index}`,
      isLead: index % 64 === 0,
      rootId: `a${index - (index % 64)}`,
    };
    // Count the fields a scan reads from every agent.
    return new Proxy(agent, {
      get(target, key) {
        if (key === "id" || key === "isLead" || key === "rootId") reads += 1;
        return Reflect.get(target, key);
      },
    }) as unknown as Agent;
  });
  const rooms = Array.from(
    { length: ROOM_COUNT },
    (_, room) =>
      ({
        id: `r${room}`,
        kind: "private",
        members: Array.from(
          { length: MEMBERS_PER_ROOM },
          (_, member) => `a${(room * 7 + member * 131) % TEAM_SIZE}`,
        ),
      }) as unknown as Room,
  );
  const expected = rooms.map((room) => scanLeadIds(room, agents));
  reads = 0;
  const actual = rooms.map((room) => roomLeadIds(room, agents));
  expect(actual).toEqual(expected);
  // One index pass over the team, then two reads per room member: the cost
  // follows the rooms, not rooms x team (the scan reads 131072 ids).
  expect(reads).toBeLessThanOrEqual(
    TEAM_SIZE + ROOM_COUNT * MEMBERS_PER_ROOM * 2,
  );
});
