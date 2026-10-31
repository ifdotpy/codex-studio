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
    saveChatCreation("codex-pending-creation:workspace", first);
    saveChatCreation("codex-pending-creation:workspace", second);

    expect(
      confirmChatCreationResponse(
        "codex-pending-creation:workspace",
        "request-one",
        "different",
      ),
    ).toBe(false);
    expect(pendingChatCreations("codex-pending-creation:workspace")).toEqual([
      first,
      second,
    ]);

    expect(
      confirmChatCreationResponse(
        "codex-pending-creation:workspace",
        "request-one",
        "request-one",
      ),
    ).toBe(true);
    expect(pendingChatCreations("codex-pending-creation:workspace")).toEqual([
      second,
    ]);
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
    saveChatCreation("codex-pending-creation:workspace", request);
    expect(
      settleFailedChatCreation(
        "codex-pending-creation:workspace",
        "request-one",
        error,
        "refused",
      ),
    ).toBe(true);
    expect(pendingChatCreations("codex-pending-creation:workspace")).toEqual(
      [],
    );
    expect(rows.size).toBe(0);
  });

  it.each([
    ["network failure", new TypeError("Failed to fetch")],
    ["client timeout", new NetworkTimeoutError()],
    ["request timeout status", new ApiError("Request timeout", 408)],
    ["server error", new ApiError("Internal server error", 500)],
    ["unavailable server", new ApiError("Unavailable", 503)],
  ])("keeps the pending request after a lost response: %s", (_, error) => {
    saveChatCreation("codex-pending-creation:workspace", request);
    expect(
      settleFailedChatCreation(
        "codex-pending-creation:workspace",
        "request-one",
        error,
        "lost",
      ),
    ).toBe(false);
    expect(pendingChatCreations("codex-pending-creation:workspace")).toEqual([
      request,
    ]);
  });

  it("dismisses an unknown outcome without archiving its body", () => {
    const other: Json = { id: "request-two", cwd: "/project/two" };
    saveChatCreation("codex-pending-creation:workspace", request);
    saveChatCreation("codex-pending-creation:workspace", other);
    dismissChatCreation("codex-pending-creation:workspace", "request-one");
    expect(pendingChatCreations("codex-pending-creation:workspace")).toEqual([
      other,
    ]);
    expect(rows.size).toBe(1);
  });
});
