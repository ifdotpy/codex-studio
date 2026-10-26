import { beforeEach, describe, expect, it, vi } from "vitest";
import { mergePreferenceFields, preferenceCacheKey } from "./uiPreferenceMerge";
import {
  preferenceScope,
  storageFields,
  prunePreferenceFields,
  type PreferenceFields,
} from "./uiPreferenceMerge";
import policyCases from "../../../../contracts/preference-policy-cases.json";
import { MODEL_WHITELIST_PREFERENCE_KEY } from "../generated/sharedValues";
import {
  capturePreferenceWrite,
  acknowledgePreferenceFields,
  readPreferenceFields,
  receivePreferenceFields,
  initializePreferenceMigration,
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

describe("legacy preference parity with the canonical Rust client owner", () => {
  it("removes only an acknowledged omitted raw property", () => {
    const storage = memoryStorage();
    const key = "codex-project-tree:/state";
    const name = JSON.stringify([key, "item"]);
    const submitted: PreferenceFields = {
      [name]: { value: true, timestamp: 2, writer: "local" },
    };
    storage.setItem(key, JSON.stringify({ item: true, sibling: true }));
    storage.setItem(`${preferenceCacheKey}:server`, JSON.stringify(submitted));
    acknowledgePreferenceFields("server", submitted, {}, storage);
    expect(JSON.parse(storage.getItem(key)!)).toEqual({ sibling: true });
    expect(readPreferenceFields("server", storage)).toEqual({});
  });
  it("preserves exact-field ACK correction and complete-map omission", () => {
    const cases: Array<{
      name: string;
      current: PreferenceFields;
      submitted: PreferenceFields;
      accepted: PreferenceFields;
      expected: PreferenceFields;
    }> = JSON.parse(JSON.stringify(policyCases.acknowledgements));
    for (const entry of cases) {
      const storage = memoryStorage();
      storage.setItem(
        `${preferenceCacheKey}:server`,
        JSON.stringify(entry.current),
      );
      acknowledgePreferenceFields(
        "server",
        entry.submitted,
        entry.accepted,
        storage,
      );
      expect(readPreferenceFields("server", storage), entry.name).toEqual(
        entry.expected,
      );
    }
  });
  it("runs every shared scope, field, merge, edit and pruning case", () => {
    // Deserialize tracked JSON without TypeScript's synthetic optional union keys.
    const mergeCases: Array<{
      name: string;
      prior: PreferenceFields;
      incoming: PreferenceFields;
      expected: PreferenceFields;
    }> = JSON.parse(JSON.stringify(policyCases.merges));
    expect(policyCases.owner).toBe(
      "workspaces/client/packages/client-domain/src/preferences.rs",
    );
    expect(policyCases.legacyConsumers).toEqual([
      "workspaces/client/apps/web/src/sync/uiPreferenceMerge.ts",
      "workspaces/client/apps/web/src/sync/uiPreferenceStore.ts",
    ]);
    for (const entry of policyCases.scopes)
      expect(
        preferenceScope(
          entry.key === "$modelWhitelistKey"
            ? MODEL_WHITELIST_PREFERENCE_KEY
            : entry.key,
        ),
      ).toBe(entry.expected);
    for (const entry of policyCases.storage)
      expect(storageFields(entry.key, entry.raw)).toEqual(entry.expected);
    for (const entry of mergeCases)
      expect(
        mergePreferenceFields(entry.prior, entry.incoming),
        entry.name,
      ).toEqual(entry.expected);
    for (const entry of policyCases.edits) {
      const storage = memoryStorage();
      const key = "codex-project-tree:/state";
      storage.setItem(key, JSON.stringify(entry.latest));
      expect(
        writePreferenceEdit<Record<string, unknown>>(
          key,
          entry.previous,
          entry.next,
          storage,
        ),
      ).toEqual(entry.expected);
    }
    const item = (index: number) =>
      JSON.stringify(["codex-worker-disclosures", String(index)]);
    const fields: PreferenceFields = {
      ...Object.fromEntries(
        Array.from({ length: policyCases.prune.itemCount }, (_, index) => [
          item(index),
          { value: true, timestamp: index, writer: "a" },
        ]),
      ),
      ...policyCases.prune.protected,
      ...policyCases.prune.ignored,
    };
    const pruned = prunePreferenceFields(fields);
    expect(Object.keys(pruned)).toHaveLength(policyCases.prune.expectedCount);
    expect(pruned[item(policyCases.prune.oldestRetained - 1)]).toBeUndefined();
    expect(pruned[item(policyCases.prune.oldestRetained)]).toBeDefined();
    expect(pruned[item(policyCases.prune.newestRetained)]).toBeDefined();
    for (const [key, field] of Object.entries(policyCases.prune.protected))
      expect(pruned[key]).toEqual(field);
    for (const key of Object.keys(policyCases.prune.ignored))
      expect(pruned[key]).toBeUndefined();
  });
});
describe("UI preference cache", () => {
  it("keeps one secure writer identity on HTTP without crypto.randomUUID", () => {
    const getRandomValues = vi.fn((bytes: Uint8Array) => bytes.fill(0x12));
    vi.stubGlobal("crypto", { getRandomValues });
    try {
      const before = { theme: "auto", mainFontSize: 14 };
      writePreferenceEdit("codex-studio-preferences-v1", before, {
        ...before,
        theme: "dark",
      });
      const first = readPreferenceFields("user");
      const writer = Object.values(first)[0].writer;
      expect(writer).toMatch(
        /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/,
      );
      writePreferenceEdit("codex-studio-preferences-v1", before, {
        ...before,
        mainFontSize: 18,
      });
      expect(
        Object.values(readPreferenceFields("user")).every(
          (field) => field.writer === writer,
        ),
      ).toBe(true);
      expect(getRandomValues).toHaveBeenCalledTimes(1);
    } finally {
      vi.unstubAllGlobals();
    }
  });

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
  it("drops invalid legacy appearance fields before syncing the remaining choices", () => {
    localStorage.setItem(
      "codex-studio-preferences-v1",
      JSON.stringify({ theme: "dark", sidebarFontSize: 99 }),
    );

    initializePreferenceMigration("user");

    expect(readPreferenceFields("user")).toEqual({
      '["codex-studio-preferences-v1","theme"]': expect.objectContaining({
        value: "dark",
      }),
    });
    expect(localStorage.getItem(`${preferenceCacheKey}:user`)).not.toContain(
      "sidebarFontSize",
    );
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
    capturePreferenceWrite("codex-project-compact:/state", "true", null);
    const first = readPreferenceFields("server");
    capturePreferenceWrite("codex-project-compact:/state", "true", "true");
    capturePreferenceWrite("codex.terminal.height", "800", "300");
    expect(readPreferenceFields("server")).toEqual(first);
  });
});

it("adopts the server clock correction without replacing a newer local edit", () => {
  const key = '["codex-studio-preferences-v1","theme"]';
  const sent = { [key]: { value: "dark", timestamp: 9_000_000, writer: "a" } };
  const accepted = { [key]: { ...sent[key], timestamp: 1000 } };
  receivePreferenceFields("user", sent);
  acknowledgePreferenceFields("user", sent, accepted);
  expect(readPreferenceFields("user")[key].timestamp).toBe(1000);
  receivePreferenceFields("user", {
    [key]: { ...sent[key], value: "light", timestamp: 2000 },
  });
  acknowledgePreferenceFields("user", sent, accepted);
  expect(readPreferenceFields("user")[key].value).toBe("light");
});

it("shows the server value when it wins over a clamped edit", () => {
  const key = '["codex-studio-preferences-v1","theme"]';
  const sent = { [key]: { value: "dark", timestamp: 9_000_000, writer: "a" } };
  receivePreferenceFields("user", sent);
  acknowledgePreferenceFields("user", sent, {
    [key]: { value: "light", timestamp: 2000, writer: "b" },
  });
  expect(
    JSON.parse(localStorage.getItem("codex-studio-preferences-v1")!).theme,
  ).toBe("light");
});
