import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ApiError,
  errorText,
  NetworkTimeoutError,
  READ_DEADLINE_MS,
  apiDownload,
  get,
  post,
  refreshSession,
  setToken,
  setWorkspace,
  syncGet,
  untilAnswered,
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
      Response.json(
        { error: "Monitor log is unavailable", location: ["query", "id"] },
        { status: 403 },
      ),
    );
    vi.stubGlobal("fetch", fetch);

    const request = apiDownload("/api/monitor/log", { id: "monitor-1" });
    await expect(request).rejects.toMatchObject({
      name: "Error",
      status: 403,
      location: ["query", "id"],
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
    const [resolveOlder, resolveNewer] = resolvers;
    if (!resolveOlder || !resolveNewer)
      throw new Error("Both requests must have started.");
    resolveNewer(Response.json({ token: "newer-token" }));
    await expect(newer).resolves.toMatchObject({ token: "newer-token" });
    resolveOlder(Response.json({ token: "older-token" }));
    await expect(older).resolves.toMatchObject({ token: "newer-token" });
  });

  it("waits for a slow sync read until the fixed deadline", async () => {
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
    // A slow answer still arrives inside the deadline.
    await vi.advanceTimersByTimeAsync(30000);
    expect(fetch.mock.calls[0]?.[0].signal.aborted).toBe(false);
    await vi.advanceTimersByTimeAsync(READ_DEADLINE_MS - 30000);
    await rejection;
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("sends the same write again after a timeout and stops at a real answer", async () => {
    let attempt = 0;
    const result = await untilAnswered(async () => {
      if (++attempt < 3) throw new NetworkTimeoutError();
      return "created";
    });
    expect(result).toBe("created");
    expect(attempt).toBe(3);
    const refused = new ApiError("Refused", 400);
    let calls = 0;
    await expect(
      untilAnswered(async () => {
        calls++;
        throw refused;
      }),
    ).rejects.toBe(refused);
    expect(calls).toBe(1);
    // A lost connection keeps its uncertainty for the caller.
    const lost = new TypeError("Failed to fetch");
    await expect(
      untilAnswered(async () => {
        throw lost;
      }),
    ).rejects.toBe(lost);
  });

  it.each([
    [400, { message: "Rejected" }, "Rejected"],
    [
      429,
      { error: "Provider quota exhausted", location: ["body", "account_key"] },
      "Provider quota exhausted",
    ],
    [401, { error: "Sign in again" }, "Sign in again"],
    [
      403,
      { error: "Permission denied", location: ["query", "id"] },
      "Permission denied",
    ],
    [409, "Request conflict", "Request conflict"],
    [500, null, "Request failed (500)"],
  ])(
    "preserves HTTP %i diagnostics without retrying",
    async (status, body, message) => {
      const fetch = vi.fn(async (_request: Request) =>
        Response.json(body, { status }),
      );
      vi.stubGlobal("fetch", fetch);
      const error = await get("/api/session").catch(
        (reason: unknown) => reason,
      );
      expect(error).toBeInstanceOf(ApiError);
      expect(error).toMatchObject({ status, message });
      expect((error as ApiError).location).toEqual(
        body && typeof body === "object" && "location" in body
          ? body.location
          : undefined,
      );
      expect(fetch).toHaveBeenCalledTimes(1);
      expect(fetch.mock.calls[0]?.[0]?.method).toBe("GET");
    },
  );
});

it("shows rejected field paths without echoing submitted values", () => {
  expect(
    errorText(
      new ApiError("Invalid request", 400, { outcome: "not_applied" }, [
        "body",
        "model",
      ]),
    ),
  ).toBe("Invalid request (body.model).");
  expect(errorText(new ApiError("Invalid request", 400, {}))).toBe(
    "Invalid request",
  );
});
