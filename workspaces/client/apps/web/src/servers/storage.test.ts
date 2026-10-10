import { expect, it } from "vitest";
import { scopedStorage } from "./storage";
function memoryStorage(): Storage {
  const rows = new Map<string, string>();
  return {
    get length() {
      return rows.size;
    },
    key: (i) => [...rows.keys()][i] ?? null,
    getItem: (key) => rows.get(key) ?? null,
    setItem: (key, value) => {
      rows.set(key, value);
    },
    removeItem: (key) => {
      rows.delete(key);
    },
    clear: () => rows.clear(),
  };
}
it("isolates all keys, enumeration and deletion with identical draft and state paths", () => {
  const raw = memoryStorage();
  raw.setItem("studio-preferences", "old");
  const first = scopedStorage(() => raw, "server:first:");
  const second = scopedStorage(() => raw, "server:second:");
  first.setItem("draft:/same/path:chat", "first draft");
  second.setItem("draft:/same/path:chat", "second draft");
  expect(first.getItem("draft:/same/path:chat")).toBe("first draft");
  expect(second.getItem("draft:/same/path:chat")).toBe("second draft");
  expect(first.length).toBe(1);
  expect(first.key(0)).toBe("draft:/same/path:chat");
  expect(first.key(1)).toBeNull();
  first.clear();
  expect(first.length).toBe(0);
  expect(second.length).toBe(1);
  expect(raw.getItem("studio-preferences")).toBe("old");
});
it("preserves the local installation's existing storage identity", () => {
  const raw = memoryStorage();
  raw.setItem("codex-sync-workspace", "cached-workspace");
  const local = scopedStorage(() => raw, "");
  expect(local.getItem("codex-sync-workspace")).toBe("cached-workspace");
  local.setItem("draft", "text");
  expect(raw.getItem("draft")).toBe("text");
});
