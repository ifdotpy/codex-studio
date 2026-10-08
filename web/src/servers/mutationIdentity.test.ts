import { beforeEach, expect, it, vi } from "vitest";
const rows = new Map<string, string>();
vi.mock("./environment", () => ({ serverViewId: "remote" }));
vi.mock("./storage", () => ({
  serverLocalStorage: {
    getItem: (key: string) => rows.get(key) ?? null,
    setItem: (key: string, value: string) => rows.set(key, value),
    removeItem: (key: string) => rows.delete(key),
  },
}));
import { beginServerMutation } from "./mutationIdentity";
beforeEach(() => rows.clear());
it("preserves an unresolved write across retries and releases only a confirmed write", async () => {
  const first = (await beginServerMutation("/api/settings", {
    id: "account",
    value: "one",
  }))!;
  const retry = (await beginServerMutation("/api/settings", {
    id: "account",
    value: "one",
  }))!;
  expect(retry.requestId).toBe(first.requestId);
  expect(
    (await beginServerMutation("/api/settings", {
      id: "account",
      value: "two",
    }))!.requestId,
  ).not.toBe(first.requestId);
  retry.finish();
  expect(
    (await beginServerMutation("/api/settings", {
      id: "account",
      value: "one",
    }))!.requestId,
  ).not.toBe(first.requestId);
});
it("uses caller operation identities for sends and explicit request identities", async () => {
  expect(
    (await beginServerMutation("/api/messages", { id: "send-once" }))!
      .requestId,
  ).toBe("send-once");
  expect(
    (await beginServerMutation("/api/projects", { requestId: "create-once" }))!
      .requestId,
  ).toBe("create-once");
  expect(
    (await beginServerMutation("/api/accounts", { request_id: "edit-once" }))!
      .requestId,
  ).toBe("edit-once");
  expect(rows.size).toBe(0);
});
