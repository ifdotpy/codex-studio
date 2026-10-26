import { expect, it } from "vitest";
import { roomLeadIds } from "./chatScope";
import type { Agent, Room } from "./types";

const TEAM_SIZE = 512;
const ROOM_COUNT = 256;
const MEMBERS_PER_ROOM = 4;

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
  const examples = [
    {
      members: ["a449", "a1", "a128", "a65"],
      leads: ["a0", "a64", "a128", "a448"],
    },
    { members: ["a65", "a64", "missing", "a1"], leads: ["a0", "a64"] },
    { members: ["a511", "a510", "a449", "a448"], leads: ["a448"] },
    { members: ["missing", "missing", "unknown", "unknown"], leads: [] },
  ];
  const rooms = Array.from({ length: ROOM_COUNT }, (_, index) => {
    const example = examples[index % examples.length];
    if (!example) throw new Error("A room example is required.");
    return example;
  });
  reads = 0;
  for (const { members, leads } of rooms) {
    const room = { kind: "private", members } as Room;
    expect(roomLeadIds(room, agents)).toEqual(leads);
  }
  expect(reads).toBeLessThanOrEqual(
    TEAM_SIZE + ROOM_COUNT * MEMBERS_PER_ROOM * 2,
  );
});
