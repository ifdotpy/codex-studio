import { afterEach, expect, it, vi } from "vitest";
const owner = vi.hoisted(() => ({}) as Storage);
vi.mock("../servers/ServerServicesContext", () => ({
  useServerStorage: () => owner,
}));
vi.mock("react", () => ({
  useCallback: (value: unknown) => value,
  useMemo: (value: () => unknown) => value(),
  useSyncExternalStore: (_subscribe: unknown, snapshot: () => unknown) =>
    snapshot(),
}));
import { useRemovedMessages } from "./removedMessages";
import type { Message } from "../types";

afterEach(() => vi.unstubAllGlobals());
it("hides and restores a message only in the current page", () => {
  vi.stubGlobal("window", new EventTarget());
  const message: Message = {
    id: "id",
    role: "user",
    text: "text",
    deliveryStatus: "sending",
  };
  const initial = useRemovedMessages("workspace", "agent", "chat");
  initial.remove(message);
  const hidden = useRemovedMessages("workspace", "agent", "chat");
  expect(hidden.hidden(message)).toBe(true);
  hidden.restore();
  const restored = useRemovedMessages("workspace", "agent", "chat");
  expect(restored.hidden(message)).toBe(false);
  expect(restored.restored).toMatchObject([
    { id: "id", text: "text", deliveryStatus: "uncertain" },
  ]);
});
