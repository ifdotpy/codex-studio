import { afterEach, expect, it, vi } from "vitest";
import {
  installNativeColorSchemePreferences,
  localStorageColorSchemeManager,
  NATIVE_COLOR_SCHEME_KEY,
} from "./colorSchemeManager";

let dispose: (() => void) | undefined;
afterEach(() => {
  dispose?.();
  dispose = undefined;
});
function owner() {
  const values = new Map<string, string>();
  const listeners = new Set<(key: string | null) => void>();
  return {
    values,
    listeners,
    getItem: (key: string) => values.get(key) ?? null,
    setItem: vi.fn((key: string, value: string) => {
      values.set(key, value);
      for (const listener of listeners) listener(key);
    }),
    removeItem: vi.fn((key: string) => {
      values.delete(key);
      for (const listener of listeners) listener(key);
    }),
    subscribe: (listener: (key: string | null) => void) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
  };
}
it("requires the installed native owner before constructing a manager", () => {
  expect(() => localStorageColorSchemeManager()).toThrow(/owner/);
});
it("uses native preference reads, writes and clear without browser storage", () => {
  const storage = owner();
  dispose = installNativeColorSchemePreferences(storage);
  const manager = localStorageColorSchemeManager();
  expect(manager.get("auto")).toBe("auto");
  storage.values.set(NATIVE_COLOR_SCHEME_KEY, "invalid");
  expect(manager.get("light")).toBe("light");
  manager.set("dark");
  expect(manager.get("auto")).toBe("dark");
  expect(storage.setItem).toHaveBeenCalledWith(NATIVE_COLOR_SCHEME_KEY, "dark");
  manager.clear();
  expect(storage.removeItem).toHaveBeenCalledWith(NATIVE_COLOR_SCHEME_KEY);
});
it("subscribes to native owner changes with the exact key and no retained listener", () => {
  const storage = owner();
  dispose = installNativeColorSchemePreferences(storage);
  const manager = localStorageColorSchemeManager({ key: "native-theme" });
  const update = vi.fn();
  manager.subscribe(update);
  storage.setItem("unrelated", "dark");
  expect(update).not.toHaveBeenCalled();
  storage.setItem("native-theme", "dark");
  expect(update).toHaveBeenLastCalledWith("dark");
  storage.setItem("native-theme", "bad");
  expect(update).toHaveBeenCalledTimes(1);
  manager.subscribe(update);
  expect(storage.listeners.size).toBe(1);
  manager.unsubscribe();
  expect(storage.listeners.size).toBe(0);
});
it("refuses a second owner and stale manager access after disposal", () => {
  const storage = owner();
  dispose = installNativeColorSchemePreferences(storage);
  const manager = localStorageColorSchemeManager();
  expect(() => installNativeColorSchemePreferences(owner())).toThrow(/owner/);
  dispose();
  dispose = undefined;
  expect(() => manager.get("auto")).toThrow(/owner/);
  expect(() => manager.set("dark")).toThrow(/owner/);
  expect(storage.setItem).not.toHaveBeenCalled();
});
it("removes manager listeners when the native mount releases its owner", () => {
  const storage = owner();
  dispose = installNativeColorSchemePreferences(storage);
  const manager = localStorageColorSchemeManager();
  manager.subscribe(vi.fn());
  expect(storage.listeners.size).toBe(1);
  dispose();
  dispose = undefined;
  expect(storage.listeners.size).toBe(0);
  expect(() => storage.setItem(NATIVE_COLOR_SCHEME_KEY, "light")).not.toThrow();
});
it("keeps a disposed manager invalid when the same preference object is reinstalled", () => {
  const storage = owner();
  const firstDispose = installNativeColorSchemePreferences(storage);
  const first = localStorageColorSchemeManager();
  firstDispose();
  dispose = installNativeColorSchemePreferences(storage);
  expect(() => first.get("auto")).toThrow(/owner/);
  expect(() => first.set("dark")).toThrow(/owner/);
  expect(localStorageColorSchemeManager().get("auto")).toBe("auto");
});
it("does not let an old disposer release a new installation of the same preference object", () => {
  const storage = owner();
  const firstDispose = installNativeColorSchemePreferences(storage);
  firstDispose();
  dispose = installNativeColorSchemePreferences(storage);
  const current = localStorageColorSchemeManager();
  const update = vi.fn();
  current.subscribe(update);
  firstDispose();
  expect(storage.listeners.size).toBe(1);
  current.set("dark");
  expect(update).toHaveBeenCalledWith("dark");
});
