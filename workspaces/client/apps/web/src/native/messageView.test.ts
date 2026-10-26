import { expect, it, vi } from "vitest";
import { createNativeMessageView } from "./messageView";
import type { NativeServerContext, NativeSubscriptionEvent } from "./client";

function fixture() {
  let receive!: (event: NativeSubscriptionEvent<unknown>) => void;
  const accepted = vi.fn();
  const failed = vi.fn();
  const stop = vi.fn(async () => {});
  const view = (generation = 1) => ({
    viewId: "view",
    generation,
    revision: generation,
    loaded: true,
    items: [],
    notice: "",
    before: null,
    after: null,
    historical: false,
    pageLoading: false,
    historyVersion: "v1",
    liveAgent: null,
    connection: "live",
  });
  const request = vi.fn(
    async (
      method: string,
      params: Record<string, unknown>,
    ): Promise<unknown> =>
      method === "transcript_view.open"
        ? { viewId: "view" }
        : method === "transcript_view.close"
          ? { closed: true }
          : {
              ...view(params.generation as number),
              found: method === "transcript_view.ensure",
            },
  );
  const context = {
    handle: "owned",
    serverId: "server",
    workspaceId: "a".repeat(32),
    request,
    subscribe: vi.fn(async (topic, params, callback) => {
      expect(topic).toBe("transcript-view");
      expect(params).toEqual({ viewId: "view" });
      receive = callback;
      return stop;
    }),
  } as unknown as NativeServerContext;
  const owner = createNativeMessageView(
    Promise.resolve(context),
    accepted,
    failed,
  );
  return {
    owner,
    context,
    request,
    accepted,
    failed,
    stop,
    view,
    event(value: unknown) {
      receive({ value });
    },
  };
}

it("sends display commands only through the immutable native context", async () => {
  const f = fixture();
  await f.owner.select({
    id: "lead",
    kind: "agent",
    managed: true,
    enabled: true,
    syncWorkspaceId: f.context.workspaceId,
  });
  expect(f.request.mock.calls[1]).toEqual([
    "transcript_view.select",
    {
      viewId: "view",
      generation: 1,
      workspace: f.context.workspaceId,
      id: "lead",
      kind: "agent",
      managed: true,
      enabled: true,
    },
  ]);
  await f.owner.command("older");
  await f.owner.command("newer");
  await f.owner.command("anchor", { anchor: "message" });
  expect(
    await f.owner.command("ensure", { messageId: "message" }),
  ).toMatchObject({ found: true });
  await f.owner.command("latest");
  await f.owner.command("refresh");
  expect(f.request.mock.calls.map(([method]) => method)).toEqual([
    "transcript_view.open",
    "transcript_view.select",
    "transcript_view.older",
    "transcript_view.newer",
    "transcript_view.anchor",
    "transcript_view.ensure",
    "transcript_view.latest",
    "transcript_view.refresh",
  ]);
  await f.owner.close();
  expect(f.stop).toHaveBeenCalledTimes(1);
  expect(f.request).toHaveBeenLastCalledWith("transcript_view.close", {
    viewId: "view",
  });
});

it("ignores stale selection events and preserves the current native view", async () => {
  const f = fixture();
  await f.owner.select({
    id: "A",
    kind: "agent",
    managed: true,
    enabled: true,
  });
  await f.owner.select({
    id: "B",
    kind: "agent",
    managed: true,
    enabled: true,
  });
  f.accepted.mockClear();
  f.event(f.view(1));
  expect(f.accepted).not.toHaveBeenCalled();
  f.event(f.view(2));
  expect(f.accepted).toHaveBeenCalledExactlyOnceWith(f.view(2));
  await f.owner.close();
  f.event(f.view(2));
  expect(f.accepted).toHaveBeenCalledTimes(1);
});

it("refuses a stale workspace before opening or selecting a view", async () => {
  const f = fixture();
  await expect(
    f.owner.select({
      id: "lead",
      kind: "agent",
      managed: true,
      enabled: true,
      syncWorkspaceId: "b".repeat(32),
    }),
  ).rejects.toThrow("workspace");
  expect(f.request).not.toHaveBeenCalled();
  await f.owner.close();
});

it("shows a native view error and never substitutes a browser read", async () => {
  const f = fixture();
  await f.owner.select({
    id: "lead",
    kind: "agent",
    managed: true,
    enabled: true,
  });
  f.event({ ...f.view(), viewId: "wrong" });
  expect(f.failed).toHaveBeenCalled();
  f.request.mockRejectedValueOnce(new Error("Native history unavailable"));
  await expect(f.owner.command("older")).rejects.toThrow(
    "Native history unavailable",
  );
  expect(
    f.request.mock.calls.every(([method]) =>
      method.startsWith("transcript_view."),
    ),
  ).toBe(true);
  await f.owner.close();
});

it("rejects old view revisions after a newer command response", async () => {
  const f = fixture();
  await f.owner.select({
    id: "lead",
    kind: "agent",
    managed: true,
    enabled: true,
  });
  let finish!: (value: unknown) => void;
  f.request.mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  );
  const pending = f.owner.command("older");
  await Promise.resolve();
  f.request.mockResolvedValueOnce({ ...f.view(), revision: 3 });
  await f.owner.command("newer");
  f.accepted.mockClear();
  finish({ ...f.view(), revision: 2 });
  await pending;
  f.event({ ...f.view(), revision: 2 });
  expect(f.accepted).not.toHaveBeenCalled();
  await f.owner.close();
});

it("does not report a stale command error after another navigation succeeds", async () => {
  const f = fixture();
  await f.owner.select({
    id: "lead",
    kind: "agent",
    managed: true,
    enabled: true,
  });
  let fail!: (error: unknown) => void;
  f.request.mockImplementationOnce(
    () =>
      new Promise((_resolve, reject) => {
        fail = reject;
      }),
  );
  const pending = f.owner.command("older");
  await Promise.resolve();
  await f.owner.command("newer");
  fail(new Error("Old request failed"));
  await expect(pending).resolves.toBeUndefined();
  await f.owner.close();
});

it("closes the allocated Rust view if its subscription cannot attach", async () => {
  const f = fixture();
  vi.mocked(f.context.subscribe).mockRejectedValueOnce(new Error("Subscription unavailable"));
  await expect(f.owner.select({ id: "lead", kind: "agent", managed: true, enabled: true })).rejects.toThrow("Subscription unavailable");
  expect(f.request).toHaveBeenLastCalledWith("transcript_view.close", { viewId: "view" });
});

it("closes the Rust view even if listener removal fails", async () => {
  const f = fixture();
  await f.owner.select({ id: "lead", kind: "agent", managed: true, enabled: true });
  f.stop.mockRejectedValueOnce(new Error("Listener removal failed"));
  await expect(f.owner.close()).rejects.toThrow("Listener removal failed");
  expect(f.request).toHaveBeenLastCalledWith("transcript_view.close", { viewId: "view" });
});
