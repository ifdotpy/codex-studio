import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ApiError,
  NetworkTimeoutError,
  apiDownload,
  get,
  post,
  registerSyncEntityPersister,
  refreshSession,
  setToken,
  setWorkspace,
  syncGet,
} from "./api";
import {
  getEntitySequenceCheckpoint,
  pullUnlessEntitySequenceInvalidationCovered,
} from "./sync/entitySequence";
import { API_SCHEMA_HASH } from "./generated/apiSchema";

describe("OpenAPI transport facade", () => {
  beforeEach(() => {
    setToken("test-token");
    setWorkspace("workspace-a");
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("keeps GET query and cache headers while omitting POST credentials", async () => {
    const fetch = vi.fn(async (request: Request) => {
      expect(new URL(request.url).searchParams.get("account_key")).toBe(
        "default",
      );
      return Response.json({});
    });
    vi.stubGlobal("fetch", fetch);

    await get("/api/limits", {
      query: { account_key: "default" },
      etag: "etag-old",
      readMetadata: {},
    });

    const request = fetch.mock.calls[0]?.[0];
    expect(request).toBeInstanceOf(Request);
    if (!(request instanceof Request)) throw new Error("Request missing");
    expect(request.method).toBe("GET");
    expect(request.headers.get("X-Canvas-Token")).toBeNull();
    expect(request.headers.get("X-Canvas-Workspace")).toBeNull();
    expect(request.headers.get("If-None-Match")).toBe("etag-old");
  });

  it.each([400, 500])(
    "treats an empty HTTP %i as a failure",
    async (status) => {
      const fetch = vi.fn(
        async () =>
          new Response(null, {
            status,
            headers: { "Content-Length": "0" },
          }),
      );
      vi.stubGlobal("fetch", fetch);

      await expect(get("/api/session")).rejects.toBeInstanceOf(ApiError);
      expect(fetch).toHaveBeenCalledTimes(1);
    },
  );

  it("treats a JSON null error body as a failed HTTP response", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response("null", {
            status: 500,
            headers: { "Content-Type": "application/json" },
          }),
      ),
    );

    await expect(get("/api/session")).rejects.toBeInstanceOf(ApiError);
  });

  it("preserves structured download errors after OpenAPI parsing", async () => {
    const fetch = vi.fn(async () =>
      Response.json({ error: "Monitor log is unavailable" }, { status: 403 }),
    );
    vi.stubGlobal("fetch", fetch);

    const request = apiDownload("/api/monitor/log", { id: "monitor-1" });
    await expect(request).rejects.toMatchObject({
      name: "Error",
      status: 403,
      details: { error: "Monitor log is unavailable" },
      message: "Monitor log is unavailable",
    });
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("preserves download blobs and response metadata", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(new Blob(["saved output"]), {
            status: 200,
            headers: {
              "Content-Disposition": 'attachment; filename="monitor.log"',
              "X-Log-Truncated": "true",
            },
          }),
      ),
    );

    const download = await apiDownload("/api/monitor/log", {
      id: "monitor-1",
    });

    expect(download.name).toBe("monitor.log");
    expect(download.truncated).toBe(true);
    expect(await download.blob.text()).toBe("saved output");
  });

  it("returns cached reads for 304 and updates the read metadata", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(
        async () =>
          new Response(null, { status: 304, headers: { ETag: "etag-new" } }),
      ),
    );
    const readMetadata: { etag?: string; notModified?: boolean } = {};

    const result = await get("/api/limits", {
      query: { account_key: "default" },
      etag: "etag-old",
      readMetadata,
    });

    expect(result).toBeUndefined();
    expect(readMetadata).toEqual({ etag: "etag-new", notModified: true });
  });

  it("sends a POST once with its original request identity and workspace", async () => {
    const fetch = vi.fn(async (_request: Request) =>
      Response.json({ ok: true }),
    );
    vi.stubGlobal("fetch", fetch);

    await post(
      "/api/sync/drafts",
      {
        rows: [],
      },
      { workspaceId: "workspace-b" },
    );

    expect(fetch).toHaveBeenCalledTimes(1);
    const request = fetch.mock.calls[0]?.[0];
    expect(request).toBeInstanceOf(Request);
    if (!(request instanceof Request)) throw new Error("Request missing");
    expect(request.method).toBe("POST");
    expect(request.headers.get("X-Canvas-Token")).toBe("test-token");
    expect(request.headers.get("X-Canvas-Workspace")).toBe("workspace-b");
    expect(await request.json()).toEqual({ rows: [] });
    expect(request.signal.aborted).toBe(false);
  });

  it.each([
    ["204", () => new Response(null, { status: 204 })],
    [
      "zero Content-Length",
      () =>
        new Response(null, {
          status: 200,
          headers: { "Content-Length": "0" },
        }),
    ],
    ["empty body without Content-Length", () => new Response(null)],
  ] as const)(
    "resolves an empty successful POST (%s)",
    async (_label, response) => {
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => response()),
      );

      await expect(
        post("/api/sync/drafts", { rows: [] }),
      ).resolves.toBeUndefined();
    },
  );

  it("waits for mutation entities to persist before resolving the POST", async () => {
    const events = new EventTarget();
    vi.stubGlobal("window", events);
    const documents = [
      {
        id: "entity:workspace:current",
        seq: 5,
        payload: JSON.stringify({
          collection: "workspace",
          id: "current",
          value: {
            sidebarOrder: { revision: 1, groups: { projects: ["c", "a"] } },
          },
        }),
        _deleted: false,
      },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => Response.json({ _syncEntities: documents })),
    );
    let releasePersistence = () => {};
    const persistence = new Promise<void>((resolve) => {
      releasePersistence = resolve;
    });
    let markPersisterStarted = () => {};
    const persisterStarted = new Promise<void>((resolve) => {
      markPersisterStarted = resolve;
    });
    const unregister = registerSyncEntityPersister(
      async (workspaceId, rows) => {
        expect(workspaceId).toBe("workspace-a");
        expect(rows).toEqual(documents);
        markPersisterStarted();
        await persistence;
      },
    );
    try {
      let settled = false;
      const request = post("/api/sync/drafts", { rows: [] }).then((result) => {
        settled = true;
        return result;
      });
      await persisterStarted;
      expect(settled).toBe(false);
      releasePersistence();
      await request;
      expect(settled).toBe(true);
    } finally {
      unregister();
    }
  });

  it("publishes contiguous mutation coverage before the held persister runs", async () => {
    vi.stubGlobal("window", new EventTarget());
    const workspaceId = "coverage-held-workspace";
    setWorkspace(workspaceId);
    const checkpoint = getEntitySequenceCheckpoint(
      workspaceId,
      "state:entities:v1",
      API_SCHEMA_HASH,
    );
    checkpoint.reset();
    checkpoint.assign(10);
    const documents = [
      { id: "entity:agent:a", seq: 12, payload: "{}", _deleted: false },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({ _syncEntities: documents, _syncEntitiesAfter: 10 }),
      ),
    );
    let releasePersistence = () => {};
    const persistence = new Promise<void>((resolve) => {
      releasePersistence = resolve;
    });
    let markStarted = () => {};
    const started = new Promise<void>((resolve) => {
      markStarted = resolve;
    });
    const unregister = registerSyncEntityPersister(async () => {
      markStarted();
      await persistence;
      checkpoint.assign(12);
    });
    try {
      const request = post("/api/sync/drafts", { rows: [] });
      await started;
      expect(checkpoint.effectiveCoverage()).toBe(12);
      const pull = vi.fn(async () => "unexpected pull");
      const result = await pullUnlessEntitySequenceInvalidationCovered(
        checkpoint.effectiveCoverage(),
        12,
        false,
        pull,
      );
      expect(result).toEqual({ skipped: true });
      checkpoint.markInFlightCoverageUsed(12);
      expect(pull).not.toHaveBeenCalled();
      releasePersistence();
      await request;
      expect(checkpoint.value).toBe(12);
      expect(checkpoint.effectiveCoverage()).toBe(12);
      await expect(
        pullUnlessEntitySequenceInvalidationCovered(
          checkpoint.effectiveCoverage(),
          12,
          false,
          pull,
        ),
      ).resolves.toEqual({ skipped: true });
      expect(pull).not.toHaveBeenCalled();
      const foreignPull = vi.fn(async () => "foreign change pulled");
      await expect(
        pullUnlessEntitySequenceInvalidationCovered(
          checkpoint.effectiveCoverage(),
          13,
          false,
          foreignPull,
        ),
      ).resolves.toEqual({ skipped: false, value: "foreign change pulled" });
      expect(foreignPull).toHaveBeenCalledOnce();
    } finally {
      unregister();
    }
  });

  it("passes an unknown watermark through without inventing one", async () => {
    vi.stubGlobal("window", new EventTarget());
    const documents = [
      {
        id: "entity:workspace:current",
        seq: 22,
        payload: "{}",
        _deleted: false,
      },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => Response.json({ _syncEntities: documents })),
    );
    const persist = vi.fn(async () => {});
    const unregister = registerSyncEntityPersister(persist);
    try {
      await post("/api/sync/drafts", { rows: [] });
      expect(persist).toHaveBeenCalledWith(
        "workspace-a",
        documents,
        undefined,
        expect.any(Function),
      );
    } finally {
      unregister();
    }
  });

  it("keeps a committed mutation result and reports persistence failures", async () => {
    vi.stubGlobal("window", new EventTarget());
    const diagnostic = vi.spyOn(console, "error").mockImplementation(() => {});
    const workspaceId = "coverage-reject-workspace";
    setWorkspace(workspaceId);
    const checkpoint = getEntitySequenceCheckpoint(
      workspaceId,
      "state:entities:v1",
      API_SCHEMA_HASH,
    );
    checkpoint.reset();
    checkpoint.assign(20);
    const documents = [
      {
        id: "entity:workspace:current",
        seq: 22,
        payload: JSON.stringify({
          collection: "workspace",
          id: "current",
          value: {},
        }),
        _deleted: false,
      },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({ _syncEntities: documents, _syncEntitiesAfter: 20 }),
      ),
    );
    const fallback = vi.fn();
    const fallbackPull = vi.fn(async () => undefined);
    checkpoint.onInFlightCoverageInvalidated(() => {
      fallback();
      void pullUnlessEntitySequenceInvalidationCovered(
        checkpoint.value,
        22,
        false,
        fallbackPull,
      );
    });
    const unregister = registerSyncEntityPersister(async () => {
      checkpoint.markInFlightCoverageUsed(22);
      throw new Error("IndexedDB write failed");
    });
    try {
      await expect(
        post("/api/sync/drafts", { rows: [] }),
      ).resolves.toMatchObject({
        _syncEntities: documents,
      });
      expect(diagnostic).toHaveBeenCalledWith(
        "Mutation entity persistence failed",
        expect.objectContaining({ workspaceId }),
      );
      expect(checkpoint.effectiveCoverage()).toBe(20);
      expect(fallback).toHaveBeenCalledOnce();
      expect(fallbackPull).toHaveBeenCalledOnce();
    } finally {
      unregister();
    }
  });

  it("bounds a stalled mutation entity persistence wait", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("window", new EventTarget());
    const workspaceId = "coverage-timeout-workspace";
    setWorkspace(workspaceId);
    const checkpoint = getEntitySequenceCheckpoint(
      workspaceId,
      "state:entities:v1",
      API_SCHEMA_HASH,
    );
    checkpoint.reset();
    checkpoint.assign(30);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({
          _syncEntitiesAfter: 30,
          _syncEntities: [
            {
              id: "entity:workspace:current",
              seq: 35,
              payload: "{}",
              _deleted: false,
            },
          ],
        }),
      ),
    );
    const diagnostic = vi.spyOn(console, "error").mockImplementation(() => {});
    const fallback = vi.fn();
    const fallbackPull = vi.fn(async () => undefined);
    checkpoint.onInFlightCoverageInvalidated(() => {
      fallback();
      void pullUnlessEntitySequenceInvalidationCovered(
        checkpoint.value,
        35,
        false,
        fallbackPull,
      );
    });
    let markStarted = () => {};
    const started = new Promise<void>((resolve) => {
      markStarted = resolve;
    });
    let releasePersistence = () => {};
    let persistenceGuard: (() => boolean) | undefined;
    const persistence = new Promise<void>((resolve) => {
      releasePersistence = resolve;
    });
    const unregister = registerSyncEntityPersister(
      async (_workspaceId, _rows, _after, isCurrent) => {
        markStarted();
        checkpoint.markInFlightCoverageUsed(35);
        persistenceGuard = isCurrent;
        await persistence;
      },
    );
    try {
      let settled = false;
      const request = post("/api/sync/drafts", { rows: [] }).then((result) => {
        settled = true;
        return result;
      });
      await started;
      await vi.advanceTimersByTimeAsync(250);
      await expect(request).resolves.toMatchObject({
        _syncEntities: expect.any(Array),
      });
      expect(settled).toBe(true);
      expect(diagnostic).toHaveBeenCalledWith(
        "Mutation entity persistence failed",
        expect.objectContaining({ error: "IndexedDB persistence timed out" }),
      );
      expect(persistenceGuard?.()).toBe(false);
      expect(checkpoint.effectiveCoverage()).toBe(30);
      expect(fallback).toHaveBeenCalledOnce();
      expect(fallbackPull).toHaveBeenCalledOnce();
      releasePersistence();
    } finally {
      unregister();
    }
  });

  it("does not apply the default read deadline to POST writes", async () => {
    vi.useFakeTimers();
    let resolveFetch: ((response: Response) => void) | undefined;
    const fetch = vi.fn(
      () =>
        new Promise<Response>((resolve) => {
          resolveFetch = resolve;
        }),
    );
    vi.stubGlobal("fetch", fetch);

    let settled = false;
    const pending = post("/api/sync/drafts", { rows: [] }).then((value) => {
      settled = true;
      return value;
    });
    await vi.advanceTimersByTimeAsync(16000);
    expect(settled).toBe(false);
    expect(fetch).toHaveBeenCalledTimes(1);

    resolveFetch?.(Response.json({ ok: true }));
    await expect(pending).resolves.toEqual({ ok: true });
  });

  it("keeps explicit timeouts and reports their distinct error type", async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(
      (request: Request) =>
        new Promise<Response>((_resolve, reject) => {
          request.signal.addEventListener(
            "abort",
            () => reject(new DOMException("Aborted", "AbortError")),
            { once: true },
          );
        }),
    );
    vi.stubGlobal("fetch", fetch);

    const pending = get("/api/session", { timeoutMs: 25 });
    const rejection =
      expect(pending).rejects.toBeInstanceOf(NetworkTimeoutError);
    await vi.advanceTimersByTimeAsync(25);
    await rejection;
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("keeps session refresh races ordered", async () => {
    const resolvers: Array<(response: Response) => void> = [];
    let onBothStarted: () => void = () => {};
    const bothStarted = new Promise<void>((resolve) => {
      onBothStarted = resolve;
    });
    const fetch = vi.fn(
      () =>
        new Promise<Response>((resolve) => {
          resolvers.push(resolve);
          if (resolvers.length === 2) onBothStarted();
        }),
    );
    vi.stubGlobal("fetch", fetch);

    const older = refreshSession();
    const newer = refreshSession();
    await bothStarted;
    expect(resolvers).toHaveLength(2);
    resolvers[1](Response.json({ token: "newer-token" }));
    await expect(newer).resolves.toMatchObject({ token: "newer-token" });
    resolvers[0](Response.json({ token: "older-token" }));
    await expect(older).resolves.toMatchObject({ token: "newer-token" });
  });

  it("uses the fixed sync read deadline", async () => {
    vi.useFakeTimers();
    const fetch = vi.fn(
      (request: Request) =>
        new Promise<Response>((_resolve, reject) => {
          const abort = () => reject(new DOMException("Aborted", "AbortError"));
          if (request.signal.aborted) abort();
          else request.signal.addEventListener("abort", abort, { once: true });
        }),
    );
    vi.stubGlobal("fetch", fetch);
    const pending = syncGet("/api/session");
    const rejection =
      expect(pending).rejects.toBeInstanceOf(NetworkTimeoutError);
    await vi.advanceTimersByTimeAsync(15000);
    await rejection;
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("uses the established ApiError class for parsed HTTP failures", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        Response.json({ message: "Rejected" }, { status: 400 }),
      ),
    );

    const error = await get("/api/session").catch((reason: unknown) => reason);
    expect(error).toBeInstanceOf(ApiError);
    expect(error).toMatchObject({ status: 400, message: "Rejected" });
  });
});
