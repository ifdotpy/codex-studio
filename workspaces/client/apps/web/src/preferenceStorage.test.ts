import { expect, it, vi } from "vitest";
import {
  installPreferenceStorage,
  preferenceStorage,
} from "./preferenceStorage";
import { serverLocalStorage } from "./servers/storage";

function memory(): Storage {
  const data = new Map<string, string>();
  return {
    get length() {
      return data.size;
    },
    key: (index) => [...data.keys()][index] ?? null,
    getItem: (key) => data.get(key) ?? null,
    setItem: (key, value) => {
      data.set(key, value);
    },
    removeItem: (key) => {
      data.delete(key);
    },
    clear: () => data.clear(),
  };
}

it("keeps preference values in memory when no native store is installed", () => {
  const browser = vi.fn(() => {
    throw new Error("browser storage must not be used");
  });
  vi.stubGlobal("localStorage", { getItem: browser, setItem: browser });
  try {
    preferenceStorage().setItem("theme", "dark");
    expect(preferenceStorage().getItem("theme")).toBe("dark");
    expect(browser).not.toHaveBeenCalled();
  } finally {
    vi.unstubAllGlobals();
  }
});

it("routes server local storage only through the installed preference store", () => {
  const browser = vi.fn(() => {
    throw new Error("browser storage must not be used");
  });
  const installed = memory();
  vi.stubGlobal("localStorage", { getItem: browser, setItem: browser });
  try {
    installPreferenceStorage(installed);
    serverLocalStorage.setItem("theme", "dark");
    expect(serverLocalStorage.getItem("theme")).toBe("dark");
    expect(installed.length).toBe(1);
    expect(browser).not.toHaveBeenCalled();
  } finally {
    vi.unstubAllGlobals();
  }
});
