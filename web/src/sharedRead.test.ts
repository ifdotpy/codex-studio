import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  get: vi.fn(),
  workspace: "workspace-a",
  observer: null as ((event: unknown) => void) | null,
}));

vi.mock("./api", () => ({
  get: mocks.get,
  getWorkspaceReadScope: () => mocks.workspace,
}));
vi.mock("./sync/resourceEvents", () => ({
  observeResourceEvents: (observer: unknown) => {
    mocks.observer = observer as (event: unknown) => void;
    return () => {};
  },
}));

import { getShared } from "./sharedRead";

describe("shared API reads", () => {
  beforeEach(() => mocks.get.mockReset());

  it("shares pending and successful responses by route and workspace", async () => {
    let resolve!: (value: { data: string[] }) => void;
    const pending = new Promise<{ data: string[] }>((done) => {
      resolve = done;
    });
    mocks.get.mockReturnValue(pending);

    const first = getShared("/api/models", {
      query: { account_key: "account-a" },
    });
    const second = getShared("/api/models", {
      query: { account_key: "account-a" },
    });
    expect(mocks.get).toHaveBeenCalledTimes(1);
    const result = { data: ["model-a"] };
    resolve(result);
    await expect(Promise.all([first, second])).resolves.toEqual([
      result,
      result,
    ]);
    await expect(
      getShared("/api/models", { query: { account_key: "account-a" } }),
    ).resolves.toBe(result);
    expect(mocks.get).toHaveBeenCalledTimes(1);

    mocks.workspace = "workspace-b";
    mocks.get.mockResolvedValue({ data: ["model-b"] });
    await expect(
      getShared("/api/models", { query: { account_key: "account-a" } }),
    ).resolves.toEqual({ data: ["model-b"] });
    expect(mocks.get).toHaveBeenCalledTimes(2);
  });

  it("shares a failure with current waiters and does not cache it", async () => {
    const failure = new Error("catalog unavailable");
    mocks.get
      .mockRejectedValueOnce(failure)
      .mockResolvedValueOnce({ data: [] });
    const options = { query: { account_key: "account-error" } } as const;
    const first = getShared("/api/models", options);
    const second = getShared("/api/models", options);
    await expect(Promise.all([first, second])).rejects.toBe(failure);
    expect(mocks.get).toHaveBeenCalledTimes(1);

    await expect(getShared("/api/models", options)).resolves.toEqual({
      data: [],
    });
    expect(mocks.get).toHaveBeenCalledTimes(2);
  });

  it("refreshes a successful value after its typed resource changes", async () => {
    mocks.get.mockResolvedValueOnce({ accountKey: "account-a", totalUSD: 1 });
    const options = { query: { account_key: "account-a" } } as const;
    const first = await getShared("/api/costs", options);
    expect(first).toEqual({ accountKey: "account-a", totalUSD: 1 });
    mocks.observer?.({
      reason: "change",
      resources: [{ kind: "costs" }],
    });
    mocks.get.mockResolvedValueOnce({ accountKey: "account-a", totalUSD: 2 });
    await expect(getShared("/api/costs", options)).resolves.toEqual({
      accountKey: "account-a",
      totalUSD: 2,
    });
    expect(mocks.get).toHaveBeenCalledTimes(2);
  });
});
