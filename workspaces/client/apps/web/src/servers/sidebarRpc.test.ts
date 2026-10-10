import { afterEach, expect, it, vi } from "vitest";
const post = vi.hoisted(() => vi.fn());
vi.mock("../api", async (original) => ({
  ...(await original<typeof import("../api")>()),
  post,
}));
import { ApiError } from "../api";
import { createSidebarRpcClient, executeSidebarRequest } from "./sidebarRpc";
import { navigationSnapshot } from "./navigation";

afterEach(() => {
  post.mockReset();
  vi.useRealTimers();
});
function surface() {
  const events = new EventTarget();
  const child = { postMessage: vi.fn() };
  const frame = {
    contentWindow: child,
    src: "https://frame.example/?studio-server=remote",
    isConnected: true,
  } as unknown as HTMLIFrameElement;
  const frames = new Map([["remote", frame]]);
  const rpc = createSidebarRpcClient(
    events as unknown as Window,
    frames,
    async () => {},
  );
  const reply = (
    data: unknown,
    source: unknown = child,
    origin = "https://frame.example",
  ) =>
    events.dispatchEvent(
      Object.assign(new Event("message"), { data, source, origin }),
    );
  return { rpc, child, frame, frames, reply };
}

it("accepts only the exact frame, origin, owner and correlation used for a request", async () => {
  const { rpc, child, reply, frames } = surface();
  const request = rpc.request("remote", { action: "refresh" });
  await Promise.resolve();
  const sent = child.postMessage.mock.calls[0][0];
  const data = {
    kind: "studio-sidebar-result",
    serverId: "remote",
    correlation: sent.correlation,
    result: { ready: true },
  };
  let resolved = false;
  void request.then(() => {
    resolved = true;
  });
  reply(data, {});
  reply(data, child, "https://wrong.example");
  reply({ ...data, serverId: "local" });
  reply({ ...data, correlation: "another-request" });
  frames.set("remote", { contentWindow: {} } as HTMLIFrameElement);
  reply(data, frames.get("remote")!.contentWindow);
  await Promise.resolve();
  expect(resolved).toBe(false);
  reply(data);
  expect(await request).toEqual({ ready: true });
  rpc.dispose();
});

it("preserves definitive API errors and uncertain timeouts without a retry", async () => {
  vi.useFakeTimers();
  const { rpc, child, reply } = surface();
  const request = rpc.request("remote", {
    action: "post",
    path: "/api/rename",
    body: { id: "same", name: "Draft", request_id: "exact" },
  });
  const rejected = request.catch((error: unknown) => error);
  await Promise.resolve();
  const sent = child.postMessage.mock.calls[0][0];
  reply({
    kind: "studio-sidebar-result",
    serverId: "remote",
    correlation: sent.correlation,
    error: { message: "Conflict", status: 409, payload: { revision: 2 } },
  });
  expect(await rejected).toMatchObject({
    status: 409,
    details: { revision: 2 },
  });
  expect(await rejected).toBeInstanceOf(ApiError);
  const unknown = rpc
    .request("remote", {
      action: "post",
      path: "/api/rename",
      body: { id: "same", name: "Draft", request_id: "exact" },
    })
    .catch((error: unknown) => error);
  await vi.advanceTimersByTimeAsync(17000);
  expect(await unknown).toBeInstanceOf(Error);
  expect(((await unknown) as Error).message).toContain("unknown");
  expect(child.postMessage).toHaveBeenCalledTimes(2);
  rpc.dispose();
});

it("uses the frame API with the exact request identity and options", async () => {
  const body = {
    action: "rename",
    path: "/remote",
    name: "Draft",
    expected_revision: 9,
    request_id: "same-request",
  };
  post.mockResolvedValue({ revision: 10 });
  const refresh = vi.fn();
  const run = vi.fn();
  expect(
    await executeSidebarRequest(
      {
        action: "post",
        path: "/api/projects",
        body,
        options: { timeoutMs: 15000, requestId: "same-request" },
      },
      refresh,
      run,
      "owner-session-token",
    ),
  ).toEqual({ revision: 10 });
  expect(post).toHaveBeenCalledExactlyOnceWith("/api/projects", body, {
    timeoutMs: 15000,
    requestId: "same-request",
    sessionToken: "owner-session-token",
  });
});

it("does not post a sidebar mutation without the owning frame session token", async () => {
  await expect(
    executeSidebarRequest(
      {
        action: "post",
        path: "/api/projects",
        body: { action: "reorder", request_id: "order-without-token" },
      },
      vi.fn(),
      vi.fn(),
    ),
  ).rejects.toThrow("The sidebar server session is not ready.");
  expect(post).not.toHaveBeenCalled();
});

it("denies non-sidebar API paths, credential options and unrelated commands", async () => {
  const refresh = vi.fn();
  const run = vi.fn();
  for (const value of [
    { action: "post", path: "/api/messages", body: { text: "No" } },
    {
      action: "post",
      path: "/api/rename",
      body: {},
      options: { sessionToken: "forbidden" },
    },
    {
      action: "post",
      path: "/api/rename",
      body: {},
      options: { timeoutMs: 60000 },
    },
    {
      action: "command",
      command: { action: "account-sign-in", provider: "codex" },
    },
  ])
    await expect(executeSidebarRequest(value, refresh, run)).rejects.toThrow(
      "Invalid sidebar",
    );
  expect(post).not.toHaveBeenCalled();
  expect(run).not.toHaveBeenCalled();
});

it("runs prefetch and unread callbacks in the owning frame and returns its fresh navigation", async () => {
  const navigation = navigationSnapshot(null, null, "");
  const refresh = vi.fn(async () => navigation);
  const run = vi.fn(async () => {});
  const command = {
    action: "mark-unread",
    id: "same",
    threadId: "thread",
    turnId: "turn",
  };
  await executeSidebarRequest({ action: "command", command }, refresh, run);
  expect(run).toHaveBeenCalledWith(command);
  await executeSidebarRequest(
    { action: "command", command: { action: "prepare-chat", id: "same" } },
    refresh,
    run,
  );
  expect(run).toHaveBeenCalledWith({ action: "prepare-chat", id: "same" });
  expect(
    await executeSidebarRequest({ action: "refresh" }, refresh, run),
  ).toEqual(navigation);
  expect(post).not.toHaveBeenCalled();
});

it("organization replies use only the sidebar agent whitelist", async () => {
  post.mockResolvedValue({
    id: "same",
    pinned: true,
    tail: "Preview",
    prompt: "PRIVATE_PROMPT",
    lastAnswer: "PRIVATE_ANSWER",
    runtime: { token: "PRIVATE_TOKEN" },
  });
  const result = await executeSidebarRequest(
    {
      action: "post",
      path: "/api/organization",
      body: { id: "same", pinned: true },
    },
    vi.fn(),
    vi.fn(),
    "owner-session-token",
  );
  expect(result).toEqual({ id: "same", pinned: true, tail: "Preview" });
});
