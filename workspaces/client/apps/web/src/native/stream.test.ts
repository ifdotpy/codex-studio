import { afterEach, expect, it, vi } from "vitest";
import { NativeEventSource } from "./stream";

afterEach(() => vi.unstubAllGlobals());

/** A fake native core that sends an event only for a granted credit. */
function setup(options: { rejectGrant?: boolean } = {}) {
  let emit: (event: { payload: unknown }) => void = () => {};
  let subscription = "";
  let acknowledge: () => void = () => {};
  const started = Promise.withResolvers<void>();
  const closed = Promise.withResolvers<void>();
  const unlisten = vi.fn();
  let credit = "credit-0";
  let granted: string | undefined;
  let sequence = 0;
  const queued: unknown[] = [];
  const flush = () => {
    if (!granted || !queued.length) return;
    const consumedCredit = granted;
    granted = undefined;
    credit = `credit-${++sequence}`;
    emit({
      payload: {
        event: subscription,
        consumedCredit,
        credit,
        value: queued.shift(),
      },
    });
  };
  const invoke = vi.fn(async (_command, { request }) => {
    const reply = (result: unknown) => ({ id: request.id, result });
    if (request.method === "view.subscribe") {
      subscription = request.params.subscription;
      started.resolve();
      return new Promise((resolve) => {
        acknowledge = () => resolve(reply({ subscribed: true }));
      });
    }
    if (request.method === "view.next") {
      if (options.rejectGrant) throw new Error("The grant response was lost.");
      if (request.params.credit !== credit || granted)
        throw new Error("Invalid credit");
      granted = credit;
      flush();
      return reply({ granted: true });
    }
    if (request.method === "view.resume" || request.method === "view.restart")
      return reply({ accepted: true });
    closed.resolve();
    return reply({ closed: true });
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
  const calls = (method: string) =>
    invoke.mock.calls
      .map((call) => call[1].request)
      .filter((request) => request.method === method);
  return {
    started: started.promise,
    closed: closed.promise,
    unlisten,
    calls,
    open: () =>
      emit({
        payload: { event: subscription, credit, value: { type: "open" } },
      }),
    event: (value: { type: string; data: string }) => {
      queued.push(value);
      flush();
    },
    control: (value: Record<string, unknown>) =>
      emit({ payload: { event: subscription, value } }),
    raw: (value: unknown, consumedCredit?: string, nextCredit = "foreign") =>
      emit({
        payload: {
          event: subscription,
          value,
          consumedCredit,
          credit: nextCredit,
        },
      }),
    acknowledge: () => acknowledge(),
  };
}

async function settle() {
  for (let index = 0; index < 5; index++) await Promise.resolve();
}

it("grants the next credit only after the page releases its hold", async () => {
  const host = setup();
  const source = new NativeEventSource("/api/sync/stream");
  const held = Promise.withResolvers<void>();
  const delivered: string[] = [];
  source.addEventListener("entity-transfer", (event) => {
    delivered.push((event as MessageEvent<string>).data);
    if (delivered.length === 1) source.holdUntil(held.promise);
  });
  await host.started;
  host.acknowledge();
  host.open();
  await settle();
  for (const data of ["one", "two", "three"])
    host.event({ type: "entity-transfer", data });
  await settle();
  expect(delivered).toEqual(["one"]);
  // Only the open credit was granted; the next one waits for the hold.
  expect(host.calls("view.next")).toHaveLength(1);
  held.resolve();
  await vi.waitFor(() => expect(delivered).toEqual(["one", "two", "three"]));
  source.close();
  await host.closed;
  expect(host.unlisten).toHaveBeenCalledOnce();
});

it("reports a core retry without unsubscribing and answers its resume request", async () => {
  const host = setup();
  const source = new NativeEventSource("/api/sync/stream?first");
  const retry = vi.fn();
  const delivered: string[] = [];
  source.onretry = retry;
  source.onresume = () => source.resume("/api/sync/stream?second");
  source.onerror = vi.fn();
  source.addEventListener("message", (event) =>
    delivered.push((event as MessageEvent<string>).data),
  );
  await host.started;
  host.acknowledge();
  host.open();
  await settle();
  host.event({ type: "message", data: "first" });
  await settle();
  vi.spyOn(Date, "now").mockReturnValue(1_000);
  host.control({
    type: "error",
    closed: false,
    opened: true,
    retryInMs: 750,
    message: "The server event stream closed.",
  });
  expect(retry).toHaveBeenCalledWith({ opened: true, retryAt: 1_750 });
  host.control({ type: "resume" });
  await settle();
  expect(host.calls("view.resume")).toEqual([
    expect.objectContaining({
      params: expect.objectContaining({ path: "/api/sync/stream?second" }),
    }),
  ]);
  // The credit granted before the drop admits the next connection's event.
  host.open();
  await settle();
  host.event({ type: "message", data: "second" });
  await settle();
  expect(delivered).toEqual(["first", "second"]);
  expect(host.calls("view.subscribe")).toHaveLength(1);
  expect(host.calls("view.unsubscribe")).toHaveLength(0);
  expect(source.onerror).not.toHaveBeenCalled();
  source.close();
});

it("drops late events after a page restart until the core reopens", async () => {
  const host = setup();
  const source = new NativeEventSource("/api/sync/stream");
  const delivered: string[] = [];
  source.addEventListener("message", (event) =>
    delivered.push((event as MessageEvent<string>).data),
  );
  await host.started;
  host.acknowledge();
  host.open();
  await settle();
  source.restart("invalid-frame: bad revision");
  host.event({ type: "message", data: "late" });
  await settle();
  expect(delivered).toEqual([]);
  expect(host.calls("view.restart")).toEqual([
    expect.objectContaining({
      params: expect.objectContaining({
        reason: "invalid-frame: bad revision",
      }),
    }),
  ]);
  // The late event consumed the only grant; the reopen grants its successor.
  expect(host.calls("view.next")).toHaveLength(1);
  host.open();
  await settle();
  expect(host.calls("view.next")).toHaveLength(2);
  host.event({ type: "message", data: "fresh" });
  await settle();
  expect(delivered).toEqual(["fresh"]);
  source.close();
});

it("stops on a terminal close from the core", async () => {
  const host = setup();
  const source = new NativeEventSource("/api/sync/stream");
  const failed = vi.fn();
  source.onerror = failed;
  await host.started;
  host.acknowledge();
  await settle();
  host.control({
    type: "error",
    closed: true,
    message: "Backend denied access",
  });
  expect(failed).toHaveBeenCalledOnce();
  await host.closed;
  expect(host.unlisten).toHaveBeenCalledOnce();
});

it("rejects a stale data credit and data without demand", async () => {
  const host = setup();
  const source = new NativeEventSource("/api/sync/stream");
  const failed = vi.fn();
  source.onerror = failed;
  await host.started;
  host.acknowledge();
  await settle();
  host.raw({ type: "message", data: "unsolicited" }, "credit-0", "credit-1");
  expect(failed).toHaveBeenCalledOnce();
  expect(host.calls("view.next")).toHaveLength(0);
});

it("closes after a lost grant response without repeating it", async () => {
  const host = setup({ rejectGrant: true });
  const source = new NativeEventSource("/api/sync/stream");
  const failed = vi.fn();
  source.onerror = failed;
  await host.started;
  host.acknowledge();
  host.open();
  await vi.waitFor(() => expect(failed).toHaveBeenCalledOnce());
  await host.closed;
  expect(host.calls("view.next")).toHaveLength(1);
});

it("unsubscribes once when closed before the subscription is acknowledged", async () => {
  const host = setup();
  const source = new NativeEventSource("/api/sync/stream");
  await host.started;
  source.close();
  host.acknowledge();
  await host.closed;
  expect(host.unlisten).toHaveBeenCalledOnce();
  expect(host.calls("view.unsubscribe")).toHaveLength(1);
});
