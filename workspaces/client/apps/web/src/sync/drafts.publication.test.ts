import { afterEach, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  setters: [] as ReturnType<typeof vi.fn>[],
}));
vi.mock("react", () => ({
  useEffect: (effect: () => void | (() => void)) => mocks.effects.push(effect),
  useRef: (current: unknown) => ({ current }),
  useCallback: (callback: unknown) => callback,
  useState: (value: unknown) => {
    const setter = vi.fn();
    mocks.setters.push(setter);
    return [typeof value === "function" ? value() : value, setter];
  },
}));
vi.mock("../api.ts", () => ({ syncPost: vi.fn() }));
vi.mock("./subscriptions/events.ts", () => ({ watchResourceChanges: vi.fn() }));
vi.mock("./resourceEvents.ts", () => ({ replaceStream: vi.fn() }));
import { createBrowserDrafts } from "./drafts.ts";
import type { ResourceVersion } from "./subscriptions/contracts.ts";

afterEach(() => {
  mocks.effects.length = 0;
  mocks.setters.length = 0;
  vi.useRealTimers();
});

it("reports an incomplete snapshot visibly", () => {
  vi.useFakeTimers();
  let receive: ((version?: ResourceVersion) => void) | undefined;
  const useDrafts = createBrowserDrafts({
    api: { syncPost: vi.fn(async () => []) },
    resources: {
      watchResourceChanges: (_ref, listener) => {
        receive = listener;
        return () => {};
      },
      replaceStream: vi.fn(),
    },
  });
  useDrafts();
  const cleanup = mocks.effects
    .map((effect) => effect())
    .filter((value) => typeof value === "function");
  receive?.({
    epoch: "e",
    revision: 3,
    reason: "initial",
    draftSnapshot: { complete: false, documents: [] },
  });
  expect(
    mocks.setters.some((setter) =>
      setter.mock.calls.some(
        ([value]) => value === "Some saved drafts could not be loaded.",
      ),
    ),
  ).toBe(true);
  cleanup.forEach((dispose) => dispose());
});

it("publishes an entire baseline before notifying either chat", () => {
  vi.useFakeTimers();
  let receive: ((version?: ResourceVersion) => void) | undefined;
  const stop = vi.fn();
  const useDrafts = createBrowserDrafts({
    api: { syncPost: vi.fn(async () => []) },
    resources: {
      watchResourceChanges: (_ref, listener) => {
        receive = listener;
        return stop;
      },
      replaceStream: vi.fn(),
    },
  });
  const drafts = useDrafts();
  const a = vi.fn(() => expect(drafts.getDraft("b")).toBe("complete"));
  const b = vi.fn(() => expect(drafts.getDraft("a")).toBe("complete"));
  drafts.subscribeDraft("a", a);
  drafts.subscribeDraft("b", b);
  const cleanup = mocks.effects
    .map((effect) => effect())
    .filter((value) => typeof value === "function");
  const row = (session: string) => ({
    id: `remote:${session}`,
    seq: 1,
    _deleted: false,
    payload: JSON.stringify({
      id: `remote:${session}`,
      device: "remote",
      session,
      text: "complete",
      updated: 1,
    }),
  });
  receive?.({
    epoch: "e",
    revision: 1,
    reason: "initial",
    draftSnapshot: { complete: true, documents: [row("a"), row("b")] },
  });
  expect(a).toHaveBeenCalledOnce();
  expect(b).toHaveBeenCalledOnce();
  cleanup.forEach((dispose) => dispose());
  expect(stop).toHaveBeenCalledOnce();
});

it("reopens on a missing interval and keeps edits until the next baseline", async () => {
  vi.useFakeTimers();
  let receive: ((version?: ResourceVersion) => void) | undefined;
  const replaceStream = vi.fn();
  const post = vi.fn(async () => []);
  const useDrafts = createBrowserDrafts({
    api: { syncPost: post },
    resources: {
      watchResourceChanges: (_ref, listener) => {
        receive = listener;
        return () => {};
      },
      replaceStream,
    },
  });
  const drafts = useDrafts();
  const cleanup = mocks.effects
    .map((effect) => effect())
    .filter((value) => typeof value === "function");
  receive?.({
    epoch: "e",
    revision: 1,
    reason: "initial",
    draftSnapshot: { complete: true, documents: [] },
  });
  drafts.setDrafts({ chat: "typed" });
  receive?.({
    epoch: "e",
    revision: 3,
    reason: "change",
    draftChanges: { after: 2, through: 3, documents: [] },
  });
  expect(replaceStream).toHaveBeenCalledOnce();
  await vi.advanceTimersByTimeAsync(3000);
  expect(post).not.toHaveBeenCalled();
  expect(drafts.getDraft("chat")).toBe("typed");
  receive?.({
    epoch: "e",
    revision: 3,
    reason: "reconnect",
    draftSnapshot: { complete: true, documents: [] },
  });
  await vi.advanceTimersByTimeAsync(3000);
  expect(post).toHaveBeenCalledOnce();
  cleanup.forEach((dispose) => dispose());
});
