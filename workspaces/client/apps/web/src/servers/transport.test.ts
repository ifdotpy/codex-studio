import { beforeEach, expect, it, vi } from "vitest";

vi.mock("./environment", () => ({
  serverViewId: "local",
  isolatedServerView: true,
}));
vi.mock("./registry", () => ({
  viewServer: () => ({ origin: "http://127.0.0.1:1234" }),
}));

import { serverFetch, setServerCredentialAdapter } from "./transport";

beforeEach(() => {
  vi.stubGlobal("location", new URL("http://studio-local.localhost:1234/"));
});

it("sends local account mutations to the frame's same origin", async () => {
  const fetch = vi.fn(async (_request: Request) =>
    Response.json({ status: "pending" }),
  );
  vi.stubGlobal("fetch", fetch);
  const request = new Request("http://127.0.0.1:1234/api/accounts/claude/add", {
    method: "POST",
    headers: { "X-Canvas-Workspace": "workspace-token" },
    body: JSON.stringify({ login_id: "request-id" }),
  });

  try {
    await serverFetch(request);

    const sent = fetch.mock.calls[0]?.[0];
    if (!sent) throw new Error("Expected the frame request");
    expect(new URL(sent.url).origin).toBe(location.origin);
    expect(new URL(sent.url).pathname).toBe("/api/accounts/claude/add");
    expect(sent.headers.get("X-Canvas-Workspace")).toBe("workspace-token");
    expect(sent.headers.get("Origin")).toBeNull();
  } finally {
    vi.unstubAllGlobals();
  }
});

it("sends local account mutations through the desktop bridge in the desktop", async () => {
  const fetch = vi.fn(async (_request: Request) => Response.json({}));
  const bridged = vi.fn(async (_server: unknown, _request: Request) =>
    Response.json({ status: "pending" }),
  );
  vi.stubGlobal("fetch", fetch);
  vi.stubGlobal("window", { codexDesktop: { platform: "darwin" } });
  setServerCredentialAdapter({
    hasPairAttempt: async () => false,
    pair: async () => {
      throw new Error("not used");
    },
    fetch: bridged,
    forget: async () => {},
  });
  const request = new Request(
    "http://127.0.0.1:1234/api/accounts/claude/login",
    {
      method: "POST",
      headers: { "X-Canvas-Token": "session-token" },
      body: JSON.stringify({ request_id: "request-id" }),
    },
  );

  try {
    await serverFetch(request);

    // The UI host refuses /api/ requests, so nothing may go to the frame origin.
    expect(fetch).not.toHaveBeenCalled();
    const sent = bridged.mock.calls[0]?.[1];
    if (!sent) throw new Error("Expected the bridged request");
    expect(new URL(sent.url).origin).toBe("http://127.0.0.1:1234");
    expect(sent.headers.get("X-Canvas-Token")).toBe("session-token");
  } finally {
    vi.unstubAllGlobals();
  }
});
