import { expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  saved: vi.fn(() => ({ old: [] })),
  update: vi.fn(),
}));
vi.mock("react", () => ({
  useRef: (current: unknown) => ({ current }),
  useState: () => [0, vi.fn()],
  useEffect: () => {},
}));
vi.mock("../servers/ServerServicesContext", () => ({
  useServerApi: () => ({ saved: mocks.saved }),
  useServerUploads: () => ({ attachmentCancelled: () => false }),
  useServerLocalDrafts: () => ({ updateLocalDraft: mocks.update }),
  useServerStorageEventKey: () => () => undefined,
}));
import { useAttachmentDrafts } from "./useAttachmentDrafts";

it("keeps attachment selections in memory", async () => {
  const [initial, set] = useAttachmentDrafts("chat", vi.fn());
  expect(initial).toEqual({});
  expect(await set({ chat: [] })).toBe(true);
  expect(mocks.saved).not.toHaveBeenCalled();
  expect(mocks.update).not.toHaveBeenCalled();
});
