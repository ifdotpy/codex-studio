import { afterEach, expect, it, vi } from "vitest";
import type { Snapshot } from "../types";

const h = vi.hoisted(() => ({ post: vi.fn(), recovery: vi.fn() }));
vi.mock("react", async (original) => ({
  ...(await original<typeof import("react")>()),
  useState: (value: unknown) => [value, vi.fn()],
  useRef: (value: unknown) => ({ current: value }),
  useCallback: (callback: unknown) => callback,
  useEffect: (effect: () => unknown) => {
    effect();
  },
}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ post: h.post }),
  useServerSync: () => ({ refreshProjection: h.recovery }),
}));
vi.mock("../sync/client", () => ({}));
import { useChatReadState } from "./useChatReadState";

afterEach(() => {
  vi.unstubAllGlobals();
  h.post.mockReset();
  h.recovery.mockReset();
});
it.each([false, true])(
  "read receipt lostResponse=%s uses canonical success or one uncertainty read",
  async (lostResponse) => {
    vi.stubGlobal("document", {
      visibilityState: "visible",
      hasFocus: () => true,
    });
    const agent = {
      id: "lead",
      threadId: "thread",
      lastCompletedTurn: "turn",
      lastCompletedTurnStatus: "completed",
      readStateSupported: true,
    };
    const snapshot = {
      stateDir: "fixture",
      threads: [agent],
    } as unknown as Snapshot;
    const refresh = vi.fn(async () => {});
    h.recovery.mockResolvedValue(snapshot);
    if (lostResponse)
      h.post.mockRejectedValue(new Error("Response unavailable"));
    else
      h.post.mockResolvedValue({
        id: "lead",
        readState: {
          threadId: "thread",
          turnId: "turn",
          revision: 1,
          read: true,
        },
      });
    const state = useChatReadState(
      snapshot,
      "lead",
      vi.fn(),
      refresh,
      "workspace",
    );
    state.observeRead({ id: "lead", threadId: "thread", turnId: "turn" });
    await vi.waitFor(() => expect(h.post).toHaveBeenCalledTimes(1));
    await Promise.resolve();
    await Promise.resolve();
    expect(refresh).not.toHaveBeenCalled();
    expect(h.recovery).toHaveBeenCalledTimes(lostResponse ? 1 : 0);
  },
);
