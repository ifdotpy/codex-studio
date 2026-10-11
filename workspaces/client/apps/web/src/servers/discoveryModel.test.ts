import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  capturePreferenceWrite,
  readPreferenceFields,
  receivePreferenceFields,
} from "../sync/uiPreferenceStore";
import { effectiveDiscoveryAliases } from "./discoveryModel";

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    key: (index) => [...values.keys()][index] ?? null,
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: (key) => values.delete(key),
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

describe("effective discovery aliases", () => {
  it("reflects local and received alias preference writes without reloading discovery", () => {
    const rawAliases = { local: "LOC" };
    const identities = { remote: "workspace-remote" };
    const key = JSON.stringify(["server-alias:workspace-remote"]);

    receivePreferenceFields("user", {
      [key]: { value: "REM", timestamp: 10, writer: "initial" },
    });
    expect(
      effectiveDiscoveryAliases(
        rawAliases,
        identities,
        readPreferenceFields("user"),
      ),
    ).toEqual({ local: "LOC", remote: "REM" });

    capturePreferenceWrite(
      "server-alias:workspace-remote",
      JSON.stringify("SRV"),
      JSON.stringify("REM"),
    );
    expect(
      effectiveDiscoveryAliases(
        rawAliases,
        identities,
        readPreferenceFields("user"),
      ),
    ).toEqual({ local: "LOC", remote: "SRV" });

    receivePreferenceFields("user", {
      [key]: {
        value: "REMOTE",
        timestamp: Date.now() + 60_000,
        writer: "received",
      },
    });
    expect(
      effectiveDiscoveryAliases(
        rawAliases,
        identities,
        readPreferenceFields("user"),
      ),
    ).toEqual({ local: "LOC", remote: "REMOTE" });
    expect(rawAliases).toEqual({ local: "LOC" });
  });
});
