import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ApiError,
  NetworkTimeoutError,
  apiDownload,
  get,
  post,
  refreshSession,
  setToken,
  setWorkspace,
  syncGet,
} from "./api";

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
      expect(new URL(request.url).searchParams.get("view")).toBe("chat");
      return Response.json({});
    });
    vi.stubGlobal("fetch", fetch);

    await get("/api/state", {
      query: { view: "chat" },
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

    const result = await get("/api/state", {
      query: { view: "chat" },
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
    "resolves an empty successful POST (%s) without dispatching sync",
    async (_label, response) => {
      const events = new EventTarget();
      const syncEvents: CustomEvent[] = [];
      events.addEventListener("codex-sync-entities", (event) => {
        syncEvents.push(event as CustomEvent);
      });
      vi.stubGlobal("window", events);
      vi.stubGlobal(
        "fetch",
        vi.fn(async () => response()),
      );

      await expect(
        post("/api/sync/drafts", { rows: [] }),
      ).resolves.toBeUndefined();
      expect(syncEvents).toEqual([]);
    },
  );

  it("dispatches sync entities from a successful POST response", async () => {
    const events = new EventTarget();
    const syncEvents: CustomEvent[] = [];
    events.addEventListener("codex-sync-entities", (event) => {
      syncEvents.push(event as CustomEvent);
    });
    vi.stubGlobal("window", events);
    const documents = [
      { id: "entity:projects:/work", seq: 4, payload: "{}", _deleted: false },
    ];
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => Response.json({ _syncEntities: documents })),
    );

    await post("/api/sync/drafts", { rows: [] });

    expect(syncEvents).toHaveLength(1);
    expect(syncEvents[0]?.detail).toEqual({
      workspaceId: "workspace-a",
      documents,
    });
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
