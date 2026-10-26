import { expect, it, vi } from "vitest";
import { createNativeMutationJournal } from "./mutationJournal";
import {
  pendingMonitorInput,
  finishMonitorInput,
} from "../servers/mutationIdentity";

it("waits for the native commit before returning a mutation identity without browser storage", async () => {
  let commit!: (value: unknown) => void;
  const request = vi.fn(
    () =>
      new Promise((resolve) => {
        commit = resolve;
      }),
  );
  const journal = createNativeMutationJournal({ request: request as any });
  let ready = false;
  const pending = journal("/api/monitor/input", {
    id: "monitor",
    text: "private",
  }).then((value) => {
    ready = true;
    return value;
  });
  expect(ready).toBe(false);
  expect(request).toHaveBeenCalledWith("monitor_input.begin", {
    monitorId: "monitor",
    serializedPayload: '{"id":"monitor","text":"private"}',
  });
  commit({
    requestId: "saved",
    payloadDigest: "a".repeat(64),
    uncertain: false,
  });
  const attempt = await pending;
  expect(ready).toBe(true);
  request.mockResolvedValue(undefined);
  await attempt!.unknown();
  expect(request).toHaveBeenLastCalledWith("monitor_input.unknown", {
    monitorId: "monitor",
    requestId: "saved",
    payloadDigest: "a".repeat(64),
  });
  await attempt!.finish();
  expect(request).toHaveBeenLastCalledWith("monitor_input.settle", {
    monitorId: "monitor",
    requestId: "saved",
    payloadDigest: "a".repeat(64),
  });
});

it("uses native pending and exact receipt settlement after reload", async () => {
  const request = vi
    .fn()
    .mockResolvedValueOnce({ requestId: "old" })
    .mockResolvedValueOnce(true);
  const journal = createNativeMutationJournal({ request });
  expect(await pendingMonitorInput(journal, "monitor")).toEqual({
    requestId: "old",
  });
  expect(await finishMonitorInput(journal, "monitor", "old")).toBe(true);
  expect(request.mock.calls).toEqual([
    ["monitor_input.pending", { monitorId: "monitor" }],
    ["monitor_input.settle", { monitorId: "monitor", requestId: "old" }],
  ]);
});

it("retains exact generic payload bytes and awaits native rejection", async () => {
  const request = vi
    .fn()
    .mockResolvedValue({
      requestId: "saved",
      payloadDigest: "b".repeat(64),
      uncertain: true,
    });
  const journal = createNativeMutationJournal({ request });
  const attempt = await journal("/api/rename", { b: 2, a: 1 });
  expect(request).toHaveBeenCalledWith("mutation.begin", {
    path: "/api/rename",
    serializedPayload: '{"b":2,"a":1}',
  });
  await attempt!.reject(503, { code: "outcome_unknown" });
  expect(request).toHaveBeenLastCalledWith("mutation.reject", {
    requestId: "saved",
    payloadDigest: "b".repeat(64),
    status: 503,
    code: "outcome_unknown",
  });
});
