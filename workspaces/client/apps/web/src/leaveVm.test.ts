import { describe, expect, it } from "vitest";
import { createApiScope } from "./api";
import { API_SCHEMA_HASH, API_SCHEMA_HASH_HEADER } from "./generated/apiSchema";
import { leaveVm, leaveVmTitle } from "./leaveVm";
import { createMutationJournal } from "./servers/mutationIdentity";
import { scopedStorage } from "./servers/storage";

function memory(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (i) => [...values.keys()][i] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => void values.set(key, value),
    removeItem: (key) => void values.delete(key),
    clear: () => values.clear(),
  };
}

function server(id: string, seen: Request[], statuses: number[]) {
  const disk = memory();
  const storage = scopedStorage(() => disk, id + ":");
  return createApiScope({
    origin: `https://${id}.example`,
    fetch: async (request) => {
      seen.push(request.clone());
      const status = statuses.shift() ?? 200;
      return new Response(
        JSON.stringify(status === 200 ? { id: "chat" } : {}),
        {
          status,
          headers: {
            [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH,
            "Content-Type": "application/json",
          },
        },
      );
    },
    localStorage: storage,
    sessionStorage: storage,
    beginMutation: createMutationJournal(id, storage),
    beginActivity: () => () => {},
  });
}

describe("Leave VM", () => {
  it("names the owning server in the tooltip", () => {
    expect(leaveVmTitle("Studio B")).toBe(
      "Move this chat from the virtual machine to Studio B",
    );
  });

  it("goes to the chat's server and a retry keeps the exact request id", async () => {
    const seenA: Request[] = [];
    const seenB: Request[] = [];
    server("a", seenA, []);
    const b = server("b", seenB, [500, 200]);
    await expect(leaveVm(b, "chat")).rejects.toThrow();
    await leaveVm(b, "chat");
    expect(seenA).toHaveLength(0);
    expect(seenB.map((r) => new URL(r.url).pathname)).toEqual([
      "/api/agents/move-to-host",
      "/api/agents/move-to-host",
    ]);
    expect(seenB.every((r) => new URL(r.url).host === "b.example")).toBe(true);
    const ids = await Promise.all(
      seenB.map(async (r) => ({
        header: r.headers.get("X-Studio-Request-Id"),
        body: (await r.json()).request_id,
      })),
    );
    expect(ids[0]?.header).toBeTruthy();
    expect(ids[0]?.body).toBe(ids[0]?.header);
    expect(ids[1]).toEqual(ids[0]);
  });
});
