import { beforeEach, expect, it, vi } from "vitest";
const h = vi.hoisted(() => ({
  effects: [] as Array<() => void | (() => void)>,
  changed: undefined as undefined | ((key: string | null) => void),
  saved: vi.fn(),
  stop: vi.fn(),
  subscribe: vi.fn(),
}));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useState: (initial: unknown) => [
    typeof initial === "function" ? initial() : initial,
    vi.fn(),
  ],
  useCallback: (callback: unknown) => callback,
  useEffect: (effect: () => void | (() => void)) => {
    h.effects.push(effect);
  },
}));
vi.mock("../../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ saved: h.saved }),
  useServerStorage: () => ({ getItem: () => null }),
  useServerUploads: () => ({ attachmentCancelled: () => false }),
  useServerLocalDrafts: () => ({ updateLocalDraft: vi.fn() }),
  useServerStorageEventKey: () => (event: { key: string | null }) => event.key,
  useServerPreferenceEvents: () => ({ subscribe: h.subscribe }),
  useServerSync: () => ({ subscribeStateProjection: () => () => {} }),
}));
vi.mock("../../api", () => ({ errorText: String, ApiError: Error }));
vi.mock("../../sync/resume", () => ({ onResume: () => () => {} }));
import { useAttachmentDrafts } from "../../components/useAttachmentDrafts";
import { useMessageQueue } from "../../components/useMessageQueue";

beforeEach(() => {
  h.effects = [];
  h.saved.mockReset().mockReturnValue({});
  h.stop.mockReset();
  h.subscribe.mockReset().mockImplementation((listener) => {
    h.changed = listener;
    return h.stop;
  });
});

it("the real attachment hook uses its owner event port without window.storage", () => {
  useAttachmentDrafts("attachments", vi.fn());
  const cleanups = h.effects.map((effect) => effect());
  expect(h.subscribe).toHaveBeenCalledOnce();
  const reads = h.saved.mock.calls.length;
  h.changed?.("another-key");
  expect(h.saved).toHaveBeenCalledTimes(reads);
  h.changed?.("attachments");
  expect(h.saved).toHaveBeenCalledTimes(reads + 1);
  for (const cleanup of cleanups) cleanup?.();
  expect(h.stop).toHaveBeenCalledOnce();
});

it("the real queue hook subscribes to its owner without a global storage event", () => {
  useMessageQueue({
    enabled: false,
    id: "chat",
    scope: "scope",
    workspaceId: "workspace",
    refresh: async () => {},
  });
  const cleanups = h.effects.map((effect) => effect());
  expect(h.subscribe).toHaveBeenCalledOnce();
  h.changed?.(null);
  for (const cleanup of cleanups) cleanup?.();
  expect(h.stop).toHaveBeenCalledOnce();
});
