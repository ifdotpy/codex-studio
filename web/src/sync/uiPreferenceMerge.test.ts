import { describe, expect, it } from "vitest";
import {
  mergePreferenceFields,
  prunePreferenceFields,
  type PreferenceFields,
  migratePreferenceFields,
  preferenceScope,
  storageFields,
} from "./uiPreferenceMerge";
function storage(values: Record<string, string>): Storage {
  return {
    get length() {
      return Object.keys(values).length;
    },
    key: (i) => Object.keys(values)[i] || null,
    getItem: (key) => values[key] ?? null,
    setItem: (key, value) => {
      values[key] = value;
    },
    removeItem: (key) => {
      delete values[key];
    },
    clear: () => {
      for (const key of Object.keys(values)) delete values[key];
    },
  };
}
describe("UI preference fields", () => {
  it("merges independent edits and rejects older versions", () => {
    const theme = { value: "dark", timestamp: 10, writer: "a" };
    const size = { value: 18, timestamp: 12, writer: "b" };
    expect(
      mergePreferenceFields(
        { theme },
        { size, theme: { ...theme, timestamp: 9, value: "light" } },
      ),
    ).toEqual({ theme, size });
  });
  it("converges at equal timestamps", () => {
    const a = { theme: { value: "light", timestamp: 10, writer: "a" } };
    const b = { theme: { value: "dark", timestamp: 10, writer: "b" } };
    expect(mergePreferenceFields(a, b)).toEqual(mergePreferenceFields(b, a));
  });
  it("migrates explicit choices once and preserves server fields", () => {
    const key = '["codex-studio-preferences-v1","theme"]';
    const prior = { [key]: { value: "dark", timestamp: 9, writer: "server" } };
    const local = storage({
      "codex-studio-preferences-v1": '{"theme":"light","mainFontSize":18}',
      "codex.terminal.height": "800",
      "codex-sidebar-collapsed": "true",
    });
    const migrated = migratePreferenceFields(local, prior, "user", "device");
    expect(migrated[key]).toEqual(prior[key]);
    expect(
      migrated['["codex-studio-preferences-v1","mainFontSize"]'].value,
    ).toBe(18);
    expect(migratePreferenceFields(local, migrated, "user", "other")).toEqual(
      migrated,
    );
    expect(Object.keys(migrated)).toHaveLength(2);
  });
  it("keeps project leaves separate and device geometry local", () => {
    expect(
      Object.keys(
        storageFields("codex-project-tree:/state", '{"a":true,"b":false}'),
      ),
    ).toHaveLength(2);
    expect(preferenceScope("codex-project-tree:/state")).toBe("server");
    expect(preferenceScope("codex.terminal.height")).toBeNull();
    expect(preferenceScope("studio-turns:/state:chat:tools-v3")).toBeNull();
    expect(preferenceScope("server-alias:workspace")).toBe("user");
  });
});

it("bounds per-item fields and keeps Appearance and server intent", () => {
  const fields: PreferenceFields = Object.fromEntries(
    Array.from({ length: 3100 }, (_, timestamp) => [
      JSON.stringify(["codex-worker-disclosures", String(timestamp)]),
      { value: true, timestamp, writer: "a" },
    ]),
  );
  fields['["codex-studio-preferences-v1","theme"]'] = {
    value: "dark",
    timestamp: 0,
    writer: "a",
  };
  fields['["server-alias:workspace"]'] = {
    value: "ABC",
    timestamp: 0,
    writer: "a",
  };
  fields['["studio-turns:/state:chat:tools-v3","turn"]'] = {
    value: true,
    timestamp: 9999,
    writer: "a",
  };
  const result = prunePreferenceFields(fields);
  expect(Object.keys(result)).toHaveLength(3002);
  expect(result['["codex-worker-disclosures","0"]']).toBeUndefined();
  expect(result['["codex-worker-disclosures","3099"]'].value).toBe(true);
  expect(result['["codex-studio-preferences-v1","theme"]'].value).toBe("dark");
});
