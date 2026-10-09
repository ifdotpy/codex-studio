import { beforeEach, describe, expect, it, vi } from "vitest";
import { mergePreferenceFields, preferenceCacheKey } from "./uiPreferenceMerge";
import {
  capturePreferenceWrite,
  readPreferenceFields,
  receivePreferenceFields,
  writePreferenceEdit,
} from "./uiPreferenceStore";
function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => {
      values.set(key, value);
    },
    removeItem: (key) => {
      values.delete(key);
    },
    clear: () => values.clear(),
  };
}
class FakeStorageEvent extends Event {
  key: string | null;
  newValue: string | null;
  constructor(type: string, options: { key?: string; newValue?: string } = {}) {
    super(type);
    this.key = options.key ?? null;
    this.newValue = options.newValue ?? null;
  }
}
beforeEach(() => {
  vi.stubGlobal("localStorage", memoryStorage());
  vi.stubGlobal("window", new EventTarget());
  vi.stubGlobal("StorageEvent", FakeStorageEvent);
});
describe("UI preference cache", () => {
  it("records only the chosen Appearance field on a first edit", () => {
    const before = { theme: "auto", mainFontSize: 14 };
    writePreferenceEdit("codex-studio-preferences-v1", before, {
      ...before,
      theme: "dark",
    });
    expect(Object.keys(readPreferenceFields("user"))).toEqual([
      '["codex-studio-preferences-v1","theme"]',
    ]);
  });
  it("keeps a remote field when a stale control changes another field", () => {
    const before = { theme: "auto", mainFontSize: 14 };
    localStorage.setItem("codex-studio-preferences-v1", JSON.stringify(before));
    receivePreferenceFields("user", {
      '["codex-studio-preferences-v1","mainFontSize"]': {
        value: 18,
        timestamp: 10,
        writer: "remote",
      },
    });
    const value = writePreferenceEdit("codex-studio-preferences-v1", before, {
      ...before,
      theme: "dark",
    });
    expect(value.mainFontSize).toBe(18);
    expect(
      readPreferenceFields("user")[
        '["codex-studio-preferences-v1","mainFontSize"]'
      ],
    ).toEqual({ value: 18, timestamp: 10, writer: "remote" });
  });
  it("merges first edits from independent clients without copying defaults", () => {
    const before = { theme: "auto", mainFontSize: 14 };
    writePreferenceEdit("codex-studio-preferences-v1", before, {
      ...before,
      theme: "dark",
    });
    const a = readPreferenceFields("user");
    localStorage.clear();
    writePreferenceEdit("codex-studio-preferences-v1", before, {
      ...before,
      mainFontSize: 18,
    });
    const merged = mergePreferenceFields(a, readPreferenceFields("user"));
    expect(
      Object.values(merged)
        .map((field) => field.value)
        .sort(),
    ).toEqual([18, "dark"]);
  });
  it("materializes a partial server preference with local defaults", () => {
    receivePreferenceFields("user", {
      '["codex-studio-preferences-v1","theme"]': {
        value: "dark",
        timestamp: 10,
        writer: "server",
      },
    });
    const cached = JSON.parse(
      localStorage.getItem("codex-studio-preferences-v1")!,
    );
    expect(cached.theme).toBe("dark");
    expect(cached.mainFontSize).toBe(14);
    expect(localStorage.getItem(`${preferenceCacheKey}:user`)).toContain(
      "server",
    );
  });
  it("does not change versions for identical values or device settings", () => {
    capturePreferenceWrite("codex-progress-hidden:/state", "true", null);
    const first = readPreferenceFields("server");
    capturePreferenceWrite("codex-progress-hidden:/state", "true", "true");
    capturePreferenceWrite("codex.terminal.height", "800", "300");
    expect(readPreferenceFields("server")).toEqual(first);
  });
});
