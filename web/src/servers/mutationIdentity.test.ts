import { beforeEach, expect, it, vi } from "vitest";
const rows = new Map<string, string>();
vi.mock("./environment", () => ({ serverViewId: "remote" }));
vi.mock("./storage", () => ({
  serverSessionStorage: {
    getItem: () => null,
    setItem() {},
    removeItem() {},
  },
  serverLocalStorage: {
    getItem: (key: string) => rows.get(key) ?? null,
    setItem: (key: string, value: string) => rows.set(key, value),
    removeItem: (key: string) => rows.delete(key),
  },
}));
vi.mock("./transport", () => ({
  apiOrigin: () => "http://localhost",
  serverFetch: (request: Request) => globalThis.fetch(request),
}));
import { beginServerMutation } from "./mutationIdentity";
import { post } from "../api";
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
it("separates operation references from durable transport receipt identities", async () => {
  const cancelBody = { action: "cancel", request_id: "transfer-operation" };
  const retryBody = { action: "retry", request_id: "transfer-operation" };
  const cancel = await beginServerMutation(
    "/api/agents/account-transfer",
    cancelBody,
  );
  const retry = await beginServerMutation(
    "/api/agents/account-transfer",
    retryBody,
  );
  const cancelRetry = await beginServerMutation(
    "/api/agents/account-transfer",
    cancelBody,
  );
  expect(cancel!.requestId).not.toBe("transfer-operation");
  expect(retry!.requestId).not.toBe("transfer-operation");
  expect(retry!.requestId).not.toBe(cancel!.requestId);
  expect(cancelRetry!.requestId).toBe(cancel!.requestId);
  expect(cancelBody.request_id).toBe("transfer-operation");
  expect(retryBody.request_id).toBe("transfer-operation");
  expect(rows.size).toBe(2);
});
it("releases a successful JSON write through the API facade after a lost response retry", async () => {
  const requests: Request[] = [];
  const fetch = vi.fn(async (request: Request) => {
    requests.push(request);
    if (requests.length === 1) throw new Error("Lost response");
    return Response.json({ rows: [] });
  });
  vi.stubGlobal("fetch", fetch);
  try {
    await expect(post("/api/sync/drafts", { rows: [] })).rejects.toThrow(
      "Lost response",
    );
    await post("/api/sync/drafts", { rows: [] });
    await post("/api/sync/drafts", { rows: [] });
    const ids = requests.map((request) =>
      request.headers.get("X-Studio-Request-Id"),
    );
    expect(ids[0]).toBeTruthy();
    expect(ids[1]).toBe(ids[0]);
    expect(ids[2]).not.toBe(ids[1]);
    expect(rows.size).toBe(0);
  } finally {
    vi.unstubAllGlobals();
  }
});
it("releases a final pre-handler HTTP failure for the next user action", async () => {
  const requests: Request[] = [];
  vi.stubGlobal("fetch", async (request: Request) => {
    requests.push(request);
    return Response.json({ error: "Validation failed" }, { status: 400 });
  });
  try {
    await expect(post("/api/sync/drafts", { rows: [] })).rejects.toMatchObject({
      status: 400,
    });
    await expect(post("/api/sync/drafts", { rows: [] })).rejects.toMatchObject({
      status: 400,
    });
    expect(requests[0].headers.get("X-Studio-Request-Id")).not.toBe(
      requests[1].headers.get("X-Studio-Request-Id"),
    );
    expect(rows.size).toBe(0);
  } finally {
    vi.unstubAllGlobals();
  }
});
it("bounds an ordinary remote POST and keeps the same identity for its retry", async () => {
  vi.useFakeTimers();
  const requests: Request[] = [];
  vi.stubGlobal("fetch", (request: Request) => {
    requests.push(request);
    if (requests.length > 1)
      return Promise.resolve(Response.json({ rows: [] }));
    return new Promise<Response>((_resolve, reject) =>
      request.signal.addEventListener("abort", () =>
        reject(request.signal.reason),
      ),
    );
  });
  try {
    const pending = post("/api/sync/drafts", { rows: [] }).then(
      () => "success",
      (error) => error.name,
    );
    await vi.waitFor(() => expect(requests).toHaveLength(1));
    await vi.advanceTimersByTimeAsync(16000);
    expect(requests[0].signal.aborted).toBe(true);
    expect(await pending).toBe("NetworkTimeoutError");
    await post("/api/sync/drafts", { rows: [] });
    expect(requests[1].headers.get("X-Studio-Request-Id")).toBe(
      requests[0].headers.get("X-Studio-Request-Id"),
    );
  } finally {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  }
});
it.each([500, 503, 504, 410])(
  "retains uncertain HTTP %i writes even after a later validation failure",
  async (status) => {
    const requests: Request[] = [];
    vi.stubGlobal("fetch", async (request: Request) => {
      requests.push(request);
      return Response.json(
        {
          error: "Not resolved",
          ...(status === 410 ? { code: "receipt_expired" } : {}),
        },
        { status: requests.length === 1 ? status : 400 },
      );
    });
    try {
      await expect(
        post("/api/sync/drafts", { rows: [] }),
      ).rejects.toBeInstanceOf(Error);
      await expect(
        post("/api/sync/drafts", { rows: [] }),
      ).rejects.toBeInstanceOf(Error);
      await expect(
        post("/api/sync/drafts", { rows: [] }),
      ).rejects.toBeInstanceOf(Error);
      expect(
        new Set(
          requests.map((request) => request.headers.get("X-Studio-Request-Id")),
        ).size,
      ).toBe(1);
      expect(rows.size).toBe(1);
    } finally {
      vi.unstubAllGlobals();
    }
  },
);
