import { describe, expect, it, vi } from "vitest";
import {
  COMMAND_CLIENT_KEY,
  createCommandClient,
  nextCommandNumber,
  type CommandRequest,
  type CommandResponse,
} from "./commands";
import { createPendingCommands } from "./pendingCommands";

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => void values.delete(key),
    setItem: (key, value) => void values.set(key, String(value)),
  };
}

const applied: CommandResponse = {
  status: "applied",
  ref_kind: "agent",
  ref_id: "chat",
  rev: 1,
  seq: 1,
  rows: [],
};

describe("command client", () => {
  it("numbers commands from 1 and keeps the client id", () => {
    const storage = memoryStorage();
    const memory = {};
    const first = nextCommandNumber(storage, memory);
    const second = nextCommandNumber(storage, memory);
    expect(first.number).toBe(1);
    expect(second.number).toBe(2);
    expect(second.client).toBe(first.client);
    expect(JSON.parse(storage.getItem(COMMAND_CLIENT_KEY)!)).toEqual(second);
  });

  it("never lets the number go down when storage is blocked", () => {
    const blocked = memoryStorage();
    vi.spyOn(blocked, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    const memory = {};
    expect(nextCommandNumber(blocked, memory).number).toBe(1);
    expect(nextCommandNumber(blocked, memory).number).toBe(2);
  });

  it("sends stop and withdraw as one command shape", async () => {
    const sent: CommandRequest[] = [];
    const client = createCommandClient(memoryStorage(), async (body) => {
      sent.push(body);
      return applied;
    });
    await client.stop("chat", false);
    await client.withdraw("chat", "message");
    expect(sent.map((command) => command.kind)).toEqual(["stop", "withdraw"]);
    expect(sent[0]).toMatchObject({
      target: "chat",
      body: { descendants: false },
      number: 1,
    });
    expect(sent[1]).toMatchObject({
      target: "chat",
      body: { message: "message" },
      number: 2,
    });
    expect(sent[0]!.id).not.toBe(sent[1]!.id);
  });

  it("retries the same command object with the same id and number", async () => {
    const sent: CommandRequest[] = [];
    let fail = true;
    const client = createCommandClient(memoryStorage(), async (body) => {
      sent.push(body);
      if (fail) {
        fail = false;
        throw new Error("lost");
      }
      return applied;
    });
    const command = client.make("stop", "chat", { descendants: true });
    await expect(client.run(command)).rejects.toThrow("lost");
    await client.run(command);
    expect(sent).toHaveLength(2);
    expect(sent[1]).toBe(sent[0]);
  });

  it("shows a pending entry at once and clears it when the client holds the row", async () => {
    const pending = createPendingCommands();
    let answer!: (response: CommandResponse) => void;
    const client = createCommandClient(
      memoryStorage(),
      () => new Promise<CommandResponse>((resolve) => (answer = resolve)),
      pending,
    );
    const running = client.withdraw("chat", "m");
    expect(pending.list()).toMatchObject([
      { kind: "withdraw", target: "chat", message: "m" },
    ]);
    answer({ ...applied, seq: 7 });
    await running;
    expect(pending.list()).toHaveLength(1);
    pending.observeClientSeq(7);
    expect(pending.list()).toHaveLength(0);
  });

  it("clears the pending entry of a command that fails", async () => {
    const pending = createPendingCommands();
    const client = createCommandClient(
      memoryStorage(),
      async () => {
        throw new Error("refused");
      },
      pending,
    );
    await expect(client.stop("chat", true)).rejects.toThrow("refused");
    expect(pending.list()).toHaveLength(0);
  });
});
