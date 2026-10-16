import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Json } from "./types";

const { rows } = vi.hoisted(() => ({ rows: new Map<string, string>() }));

vi.mock("./servers/storage", () => ({
  serverLocalStorage: {
    get length() {
      return rows.size;
    },
    key: (index: number) => [...rows.keys()][index] ?? null,
    getItem: (key: string) => rows.get(key) ?? null,
    setItem: (key: string, value: string) => rows.set(key, value),
    removeItem: (key: string) => rows.delete(key),
  },
}));

import {
  confirmChatCreationResponse,
  pendingChatCreations,
  saveChatCreation,
} from "./chatCreation";

describe("saved chat creation recovery", () => {
  beforeEach(() => rows.clear());

  it("clears only the saved request after an exact-ID success response", () => {
    const first: Json = {
      id: "request-one",
      previous: null,
      reuse_empty: false,
      cwd: "/project/one",
    };
    const second: Json = {
      id: "request-two",
      previous: null,
      reuse_empty: false,
      cwd: "/project/two",
    };
    saveChatCreation("workspace", first);
    saveChatCreation("workspace", second);

    expect(
      confirmChatCreationResponse("workspace", "request-one", "different"),
    ).toBe(false);
    expect(pendingChatCreations("workspace")).toEqual([first, second]);

    expect(
      confirmChatCreationResponse("workspace", "request-one", "request-one"),
    ).toBe(true);
    expect(pendingChatCreations("workspace")).toEqual([second]);
  });
});
