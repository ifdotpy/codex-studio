import { afterEach, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  effects: [] as (() => void | (() => void))[],
}));
vi.mock("react", () => ({
  useEffect: (effect: () => void | (() => void)) => mocks.effects.push(effect),
  useRef: (current: unknown) => ({ current }),
  useCallback: (callback: unknown) => callback,
  useState: (value: unknown) => [
    typeof value === "function" ? value() : value,
    vi.fn(),
  ],
}));
vi.mock("../native/useNativeDrafts", () => ({ useNativeDrafts: vi.fn() }));
vi.mock("../native/client", () => ({ hasNativeClient: () => false }));
vi.mock("../api", () => ({
  saved: (_key: string, value: unknown) => value,
  save: vi.fn(),
  isApiSchemaMismatch: () => false,
  onApiSchemaMismatch: () => () => {},
}));
vi.mock("./client", () => ({
  startDraftReplication: vi.fn(),
  syncDatabase: vi.fn(),
}));
vi.mock("./resume", () => ({ onResume: () => () => {} }));
import { createBrowserDrafts } from "./drafts";

let cleanup: (() => void) | undefined;
afterEach(() => {
  cleanup?.();
  cleanup = undefined;
  mocks.effects.length = 0;
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

it("publishes one complete draft query batch and cancels a pending publication on close", async () => {
  vi.useFakeTimers();
  vi.stubGlobal("document", { hidden: true });
  vi.stubGlobal("window", {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
  });
  let receive!: (documents: { getLatest: () => { payload: string } }[]) => void;
  const unsubscribe = vi.fn();
  const useDrafts = createBrowserDrafts({
    api: {
      saved: (_key, fallback) => fallback,
      save: vi.fn(),
      isApiSchemaMismatch: () => false,
      onApiSchemaMismatch: () => () => {},
    },
    sync: {
      startDraftReplication: async () => async () => {},
      syncDatabase: async () =>
        ({
          workspaceId: "unassigned",
          db: {
            drafts: {
              find: () => ({
                exec: async () => [],
                $: {
                  subscribe: (callback: typeof receive) => {
                    receive = callback;
                    return { unsubscribe };
                  },
                },
              }),
            },
          },
        }) as unknown as Awaited<
          ReturnType<
            ReturnType<
              typeof import("./client").createSyncClient
            >["syncDatabase"]
          >
        >,
    },
    journal: {
      readDraftJournal: () => [],
      writeDraftJournal: vi.fn(),
      forgetDraftJournal: vi.fn(),
    },
    storage: {
      readLocalDrafts: () => ({}),
      readLocalDraftRecord: () => null,
      writeLocalDraftRecord: vi.fn(),
      copyLocalDraftScope: vi.fn(),
    },
  });
  const drafts = useDrafts();
  const first = vi.fn(() => expect(drafts.getDraft("b")).toBe("complete"));
  const second = vi.fn(() => expect(drafts.getDraft("a")).toBe("complete"));
  drafts.subscribeDraft("a", first);
  drafts.subscribeDraft("b", second);
  for (const effect of mocks.effects) {
    const stop = effect();
    if (stop) cleanup = stop;
  }
  for (let turn = 0; turn < 20 && !receive; turn++) await Promise.resolve();
  expect(receive).toBeTypeOf("function");
  const row = (session: string, text: string) => ({
    getLatest: () => ({
      payload: JSON.stringify({
        id: `remote:${session}`,
        device: "remote",
        session,
        text,
        updated: 1,
      }),
    }),
  });
  // RxDB emits intermediate query results as the same SSE batch is written.
  receive([row("a", "partial")]);
  receive([row("a", "complete"), row("b", "complete")]);
  expect(first).not.toHaveBeenCalled();
  await vi.advanceTimersByTimeAsync(50);
  expect(first).toHaveBeenCalledOnce();
  expect(second).toHaveBeenCalledOnce();
  receive([row("a", "closed")]);
  cleanup?.();
  cleanup = undefined;
  await vi.advanceTimersByTimeAsync(50);
  expect(first).toHaveBeenCalledOnce();
  expect(unsubscribe).toHaveBeenCalledOnce();
});
