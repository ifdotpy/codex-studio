import { afterEach, expect, it, vi } from "vitest";
import { nativeEventResponse } from "./stream";

afterEach(() => vi.unstubAllGlobals());
function setup() {
  let emit: (event: { payload: unknown }) => void;
  let subscription: string;
  let acknowledge: () => void;
  let started: () => void;
  let closed: () => void;
  const startedPromise = new Promise<void>((resolve) => {
    started = resolve;
  });
  const closedPromise = new Promise<void>((resolve) => {
    closed = resolve;
  });
  const unlisten = vi.fn();
  const invoke = vi.fn(async (_command, { request }) => {
    if (request.method === "view.subscribe") {
      subscription = request.params.subscription;
      started();
      return new Promise((resolve) => {
        acknowledge = () =>
          resolve({ id: request.id, result: { subscribed: true } });
      });
    }
    closed();
    return { id: request.id, result: { closed: true } };
  });
  vi.stubGlobal("__TAURI__", {
    core: { invoke },
    event: {
      listen: async (_name: string, callback: typeof emit) => {
        emit = callback;
        return unlisten;
      },
    },
  });
  return {
    started: startedPromise,
    closed: closedPromise,
    invoke,
    unlisten,
    event: (value: unknown) =>
      emit({ payload: { event: subscription, value } }),
    acknowledge: () => acknowledge(),
  };
}
it("carries multiline Unicode events and releases the stream on cancellation", async () => {
  const host = setup();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events"),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "open" });
  const response = await result;
  const reader = response.body!.getReader();
  host.event({ type: "updated", data: "Привет 🦀\nsecond", id: "rev-1" });
  expect(new TextDecoder().decode((await reader.read()).value)).toBe(
    "id: rev-1\nevent: updated\ndata: Привет 🦀\ndata: second\n\n",
  );
  host.event({ type: "error", data: "application event", id: "rev-2" });
  expect(new TextDecoder().decode((await reader.read()).value)).toBe(
    "id: rev-2\nevent: error\ndata: application event\n\n",
  );
  await reader.cancel();
  await host.closed;
  expect(host.unlisten).toHaveBeenCalledTimes(1);
  expect(
    host.invoke.mock.calls.filter(
      (call) => call[1].request.method === "view.unsubscribe",
    ),
  ).toHaveLength(1);
});
it("closes a subscription acknowledged after the caller already aborted", async () => {
  const host = setup();
  const controller = new AbortController();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events", { signal: controller.signal }),
    "/api/events",
  );
  await host.started;
  controller.abort();
  await expect(result).rejects.toMatchObject({ name: "AbortError" });
  host.acknowledge();
  await host.closed;
  expect(host.unlisten).toHaveBeenCalledTimes(1);
  expect(
    host.invoke.mock.calls.filter(
      (call) => call[1].request.method === "view.unsubscribe",
    ),
  ).toHaveLength(1);
});
it("rejects a failed stream handshake without reporting an HTTP success", async () => {
  const host = setup();
  const result = nativeEventResponse(
    new Request("http://localhost/api/events"),
    "/api/events",
  );
  await host.started;
  host.acknowledge();
  host.event({ type: "error", message: "Backend denied access" });
  await expect(result).rejects.toThrow("Backend denied access");
  expect(host.unlisten).toHaveBeenCalledTimes(1);
});
