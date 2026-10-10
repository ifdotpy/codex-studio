import { describe, expect, it, vi } from "vitest";
import type { GetResult } from "../api";
import { drainRoomUpdates } from "./roomUpdates";

type Response = GetResult<"/api/agent-chat">;

function page(from: number, length: number, hasMore: boolean): Response {
  const messages = Array.from({ length }, (_, index) => {
    const seq = from + index + 1;
    return {
      id: `message-${seq}`,
      seq,
      room: "feed:team",
      sender: "agent",
      text: `message ${seq}`,
      created: seq,
      deliveries: {},
      senderName: "Agent",
    };
  });
  return {
    room: { id: "feed:team", kind: "broadcast", name: "Team", members: [] },
    messages,
    nextBefore: null,
    nextAfter: hasMore ? messages.at(-1)?.seq || null : null,
  };
}

describe("drainRoomUpdates", () => {
  it("reads every page after one notification until the room is current", async () => {
    const responses = [
      page(0, 100, true),
      page(100, 100, true),
      page(200, 50, false),
    ];
    const read = vi.fn(async (after: number) => {
      const response = responses.shift();
      if (!response) throw new Error(`Unexpected read after ${after}`);
      return response;
    });
    const result = await drainRoomUpdates([], 0, read, () => true);

    expect(read.mock.calls.map(([after]) => after)).toEqual([0, 100, 200]);
    expect(result?.items).toHaveLength(250);
    expect(result?.checkpoint).toBe(250);
  });

  it("stops immediately when the active room changes", async () => {
    let current = true;
    const read = vi.fn(async () => {
      current = false;
      return page(0, 100, true);
    });

    expect(await drainRoomUpdates([], 0, read, () => current)).toBeNull();
    expect(read).toHaveBeenCalledTimes(1);
  });

  it("rejects a cursor that cannot make forward progress", async () => {
    await expect(
      drainRoomUpdates(
        [],
        100,
        async () => page(99, 1, true),
        () => true,
      ),
    ).rejects.toThrow("cursor did not advance");
  });
});
