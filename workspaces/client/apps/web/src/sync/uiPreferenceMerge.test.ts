import assert from "node:assert/strict";
import { describe, expect, it } from "vitest";
import {
  mergePreferenceFields,
  prunePreferenceFields,
  type PreferenceFields,
  preferenceScope,
  storageFields,
} from "./uiPreferenceMerge";
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
  assert.ok(result['["codex-worker-disclosures","3099"]'] != null);
  expect(result['["codex-worker-disclosures","3099"]'].value).toBe(true);
  assert.ok(result['["codex-studio-preferences-v1","theme"]'] != null);
  expect(result['["codex-studio-preferences-v1","theme"]'].value).toBe("dark");
});
