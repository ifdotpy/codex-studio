import { afterEach, expect, it, vi } from "vitest";
import type { Snapshot } from "../types";
import {
  beginConversationDelete,
  conversationDeleteReadback,
} from "../chatDeleteScope";

const mocks = vi.hoisted(() => ({
  refreshSession: vi.fn(),
  refreshProjection: vi.fn(),
  stateWrites: [] as unknown[],
}));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    (next: unknown) => mocks.stateWrites.push(next),
  ],
  useCallback: (callback: unknown) => callback,
  useMemo: (factory: () => unknown) => factory(),
  useEffect: vi.fn(),
  useLayoutEffect: vi.fn(),
}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({
    refreshSession: mocks.refreshSession,
    saved: (_key: string, fallback: unknown) => fallback,
    save: vi.fn(),
  }),
  useServerSync: () => ({ refreshProjection: mocks.refreshProjection }),
  useServerResources: () => ({}),
}));
vi.mock("../sync/client", () => ({}));
vi.mock("../sync/transcriptCache", () => ({}));
import { useSnapshot } from "../hooks";

const projection = {
  threads: [],
  stateDir: "/workspace",
} as unknown as Snapshot;
const workspaceId = "workspace-id";
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((finish) => {
    resolve = finish;
  });
  return { promise, resolve };
}
afterEach(() => {
  vi.clearAllMocks();
  mocks.stateWrites.length = 0;
});

it.each(["session", "projection"] as const)(
  "settles delete readback when a credential refresh supersedes its %s request",
  async (boundary) => {
    const session = deferred<{ token: string }>();
    const read = deferred<Snapshot>();
    const started = deferred<void>();
    mocks.refreshSession.mockResolvedValue({ token: "token" });
    if (boundary === "session")
      mocks.refreshSession.mockReturnValueOnce(session.promise);
    mocks.refreshProjection.mockImplementation(
      ({ onWorkspaceId }: { onWorkspaceId: (id: string) => void }) => {
        onWorkspaceId(workspaceId);
        started.resolve();
        return read.promise;
      },
    );
    const hook = useSnapshot();
    const pending = beginConversationDelete(
      "lead",
      [{ id: "lead" }],
      "/workspace",
      workspaceId,
      () => {},
    );
    let modal = "Delete result unconfirmed";
    const readback = hook.refresh(false, (next, identity) => {
      if (
        conversationDeleteReadback(
          next.threads,
          pending.scope,
          pending,
          next.stateDir,
          identity,
        ) === "confirmed"
      )
        modal = "";
    });
    if (boundary === "projection") await started.promise;
    await hook.refresh(true);
    session.resolve({ token: "token" });
    read.resolve(projection);
    await readback;
    expect(modal).toBe("");
    expect(mocks.refreshProjection).toHaveBeenCalledTimes(1);
    expect(mocks.stateWrites).not.toContainEqual(
      expect.objectContaining({ threads: [] }),
    );
  },
);

it("keeps a mismatched workspace readback unconfirmed", async () => {
  mocks.refreshSession.mockResolvedValue({ token: "token" });
  mocks.refreshProjection.mockImplementation(
    ({ onWorkspaceId }: { onWorkspaceId: (id: string) => void }) => {
      onWorkspaceId("different-workspace");
      return Promise.resolve(projection);
    },
  );
  const hook = useSnapshot();
  const pending = beginConversationDelete(
    "lead",
    [{ id: "lead" }],
    "/workspace",
    workspaceId,
    () => {},
  );
  let result = "unavailable";
  await hook.refresh(false, (next, identity) => {
    result = conversationDeleteReadback(
      next.threads,
      pending.scope,
      pending,
      next.stateDir,
      identity,
    );
  });
  expect(result).toBe("unavailable");
});
