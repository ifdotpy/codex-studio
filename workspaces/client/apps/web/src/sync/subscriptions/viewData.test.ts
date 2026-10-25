import { expect, it, vi } from "vitest";
import { createDataObserver } from "./data";
import type { ResourceRef } from "./contracts";

it.each([
  [{ kind: "processes", agentId: "lead" }, "processes", { processes: [] }],
  [
    { kind: "complaint", complaintId: "message" },
    "complaint",
    { id: "message", text: "Reply" },
  ],
  [{ kind: "work", agentId: "lead" }, "work", { items: [], rootId: "lead" }],
  [
    { kind: "room", roomId: "team" },
    "room",
    { messages: [{ id: "new", seq: 2 }] },
  ],
  [
    { kind: "terminal-output", terminalId: "shell" },
    "terminalOutput",
    { text: "new output", offset: 10, status: "running", truncated: false },
  ],
] as const)(
  "%j consumes initial and live values with zero reads",
  async (ref, field, value) => {
    const get = vi.fn();
    let receive: (version: any) => void = () => {};
    const stop = vi.fn();
    const events = {
      watchResourceConnection: () => () => {},
      watchResourceChanges: vi.fn((_ref: unknown, accept: typeof receive) => {
        receive = accept;
        return stop;
      }),
    };
    const accept = vi.fn();
    const dispose = createDataObserver(
      () => ({ get }) as any,
      events as any,
      undefined,
      get,
    )(ref as ResourceRef, accept, vi.fn());
    receive({ epoch: "server", revision: 1, viewSnapshot: { [field]: value } });
    await vi.waitFor(() => expect(accept).toHaveBeenCalledTimes(1));
    receive({ epoch: "server", revision: 2, viewSnapshot: { [field]: value } });
    await vi.waitFor(() => expect(accept).toHaveBeenCalledTimes(2));
    expect(accept).toHaveBeenNthCalledWith(1, value);
    expect(accept).toHaveBeenNthCalledWith(2, value);
    expect(get).not.toHaveBeenCalled();
    dispose();
    expect(stop).toHaveBeenCalledOnce();
  },
);

it("an oversized view recovers once and subsequent complete frames resume zero-read delivery", async () => {
  let receive: (version: any) => void = () => {};
  const events = {
    watchResourceChanges: (_ref: unknown, accept: typeof receive) => {
      receive = accept;
      return () => {};
    },
    watchResourceConnection: () => () => {},
  };
  const read = vi.fn(async () => ({ items: [], rootId: "lead" }));
  const accept = vi.fn();
  const stop = createDataObserver(
    () => ({}) as any,
    events,
    undefined,
    read,
  )({ kind: "work", agentId: "lead" }, accept, vi.fn());
  expect(read).not.toHaveBeenCalled();
  receive({ epoch: "server", revision: 1 });
  await vi.waitFor(() => expect(accept).toHaveBeenCalledOnce());
  receive({
    epoch: "server",
    revision: 2,
    viewSnapshot: { work: { items: [], rootId: "lead" } },
  });
  await vi.waitFor(() => expect(accept).toHaveBeenCalledTimes(2));
  expect(read).toHaveBeenCalledOnce();
  stop();
});
