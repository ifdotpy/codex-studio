import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Json } from "./types";

const { rows } = vi.hoisted(() => ({ rows: new Map<string, string>() }));

vi.mock(import("./servers/storage"), async (importOriginal) => ({
  ...(await importOriginal()),
  serverLocalStorage: {
    get length() {
      return rows.size;
    },
    key: (index: number) => [...rows.keys()][index] ?? null,
    getItem: (key: string) => rows.get(key) ?? null,
    setItem: (key: string, value: string) => rows.set(key, value),
    removeItem: (key: string) => rows.delete(key),
  } as unknown as Storage,
}));

import { ApiError, NetworkTimeoutError } from "./api";
import {
  confirmChatCreationResponse,
  dismissChatCreation,
  pendingChatCreations,
  saveChatCreation,
  settleFailedChatCreation,
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

  const request: Json = {
    id: "request-one",
    previous: null,
    reuse_empty: false,
    cwd: "/project/one",
  };

  it.each([
    [
      "explicit not-applied outcome",
      new ApiError("Refused", 400, { outcome: "not_applied" }),
    ],
    ["client-error refusal", new ApiError("Model catalog is unavailable", 400)],
    ["schema mismatch", new ApiError("Update Studio", 426)],
  ])("clears the pending request after a %s", (_, error) => {
    saveChatCreation("workspace", request);
    expect(
      settleFailedChatCreation("workspace", "request-one", error, "refused"),
    ).toBe(true);
    expect(pendingChatCreations("workspace")).toEqual([]);
    expect(JSON.parse(rows.get("workspace:rejected:request-one")!)).toEqual({
      request,
      outcome: "not_applied",
      reason: "refused",
    });
  });

  it.each([
    ["network failure", new TypeError("Failed to fetch")],
    ["client timeout", new NetworkTimeoutError()],
    ["request timeout status", new ApiError("Request timeout", 408)],
    ["server error", new ApiError("Internal server error", 500)],
    ["unavailable server", new ApiError("Unavailable", 503)],
  ])("keeps the pending request after a lost response: %s", (_, error) => {
    saveChatCreation("workspace", request);
    expect(
      settleFailedChatCreation("workspace", "request-one", error, "lost"),
    ).toBe(false);
    expect(pendingChatCreations("workspace")).toEqual([request]);
  });

  it("dismisses an unknown outcome and keeps its exact body as evidence", () => {
    const other: Json = { id: "request-two", cwd: "/project/two" };
    saveChatCreation("workspace", request);
    saveChatCreation("workspace", other);
    dismissChatCreation("workspace", "request-one");
    expect(pendingChatCreations("workspace")).toEqual([other]);
    expect(JSON.parse(rows.get("workspace:dismissed:request-one")!)).toEqual({
      request,
      outcome: "unknown",
      reason: "Dismissed by the user",
    });
  });
});
