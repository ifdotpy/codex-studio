import { afterEach, expect, it, vi } from "vitest";
import type { ResourceVersion } from "../sync/resourceEvents";

const mocks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
  states: [] as unknown[],
  changes: undefined as undefined | ((version?: ResourceVersion) => void),
  post: vi.fn(),
  connection: vi.fn(() => () => {}),
}));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useEffect: (effect: () => void | (() => void)) => mocks.effects.push(effect),
  useRef: (current: unknown) => ({ current }),
  useState: (initial: unknown) => {
    const index = mocks.states.length;
    const value = index === 0 ? true : initial;
    mocks.states.push(value);
    return [
      value,
      (next: unknown) => {
        mocks.states[index] =
          typeof next === "function" ? next(mocks.states[index]) : next;
      },
    ];
  },
}));
vi.mock("@mantine/core", () => ({
  ActionIcon: () => null,
  Popover: () => null,
}));
vi.mock("../native/client", () => ({ hasNativeClient: () => false }));
vi.mock("../servers/ServerServicesContext", async () => {
  const { watchResourceReads } = await import("./watchResourceReads");
  return {
    useServerApi: () => ({ post: mocks.post }),
    useServerAudio: () => ({}),
    useServerResources: () => ({ watchResourceConnection: mocks.connection }),
    useServerResourceReads: () => watchResourceReads,
  };
});
vi.mock("../sync/resourceEvents", () => ({
  watchResourceChanges: (resource: unknown, callback: typeof mocks.changes) => {
    expect(resource).toEqual({ kind: "voice", agentId: "owner-chat" });
    mocks.changes = callback;
    return () => {
      mocks.changes = undefined;
    };
  },
  watchResourceConnection: () => () => {},
}));
vi.mock("../sync/client", () => ({
  watchResourceConnection: vi.fn(() => () => {}),
}));
vi.mock("../sync/resume", () => ({ onResume: () => () => {} }));
import RealtimeVoice from "./RealtimeVoice";

let cleanups: (() => void)[] = [];
afterEach(() => {
  cleanups.forEach((cleanup) => cleanup());
  cleanups = [];
  mocks.effects.length = 0;
  mocks.states.length = 0;
  mocks.post.mockReset();
  mocks.connection.mockClear();
  expect(mocks.changes).toBeUndefined();
});

it("consumes owner voice batches, recovering gaps and changed epochs through the same owner", async () => {
  const props = { agentId: "owner-chat", notify: vi.fn() };
  const element = RealtimeVoice(props);
  (element.type as (parameters: typeof props) => unknown)(props);
  cleanups = mocks.effects.flatMap((effect) => {
    const cleanup = effect();
    return cleanup ? [cleanup] : [];
  });
  expect(mocks.post).not.toHaveBeenCalled();
  const first = { id: "first", kind: "user", text: "hello" };
  const second = { id: "second", kind: "assistant", text: "answer" };
  const emit = (
    revision: number,
    after: number,
    cursor: number,
    records: unknown[],
    epoch = "owner",
  ) => {
    mocks.changes?.({
      epoch,
      revision,
      voiceRecords: { after, cursor, records, session: null },
    } as ResourceVersion);
  };
  emit(1, 0, 1, [first]);
  await Promise.resolve();
  await Promise.resolve();
  expect(mocks.post).not.toHaveBeenCalled();
  await vi.waitFor(() => expect(mocks.states[6]).toEqual([first]));
  expect(mocks.connection).toHaveBeenCalledOnce();
  emit(2, 1, 2, [second]);
  await vi.waitFor(() => expect(mocks.states[6]).toEqual([first, second]));
  expect(mocks.post).not.toHaveBeenCalled();
  mocks.post.mockResolvedValue({
    cursor: 4,
    records: [{ id: "recovered" }],
    session: null,
  });
  emit(3, 3, 4, []);
  await vi.waitFor(() =>
    expect(mocks.post).toHaveBeenCalledWith(
      "/api/voice/records",
      { agent: "owner-chat", after: 2 },
      { timeoutMs: 15000 },
    ),
  );
  emit(1, 0, 1, [second], "restarted");
  await vi.waitFor(() => expect(mocks.states[6]).toEqual([second]));
  expect(mocks.post).toHaveBeenCalledTimes(1);
});
