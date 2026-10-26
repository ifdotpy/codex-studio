import { expect, it } from "vitest";
import { readFileSync } from "node:fs";
import {
  DEFAULT_CHECK_PROVIDER_UPDATES,
  DEFAULT_HIDE_OLD_CHATS_THRESHOLD,
  defaultStudioPreferences,
  parseStudioPreferences,
} from "./studioPreferences";

it("shows terminals for legacy preferences and preserves a saved false", () => {
  const schema: unknown = JSON.parse(
    readFileSync(
      new URL(
        "../../../../runtime/packages/runtime-http/openapi.json",
        import.meta.url,
      ),
      "utf8",
    ),
  );
  expect(schema).toMatchObject({
    components: {
      schemas: {
        AppearancePreferences: {
          properties: {
            showTerminals: {
              type: "boolean",
              default: defaultStudioPreferences.showTerminals,
            },
          },
        },
      },
    },
  });
  const { showTerminals: _, ...legacy } = defaultStudioPreferences;
  expect(parseStudioPreferences(JSON.stringify(legacy)).showTerminals).toBe(
    true,
  );
  for (const showTerminals of [false, true])
    expect(
      parseStudioPreferences(
        JSON.stringify({ ...defaultStudioPreferences, showTerminals }),
      ).showTerminals,
    ).toBe(showTerminals);
  for (const showTerminals of [null, "false", 0])
    expect(() =>
      parseStudioPreferences(
        JSON.stringify({ ...defaultStudioPreferences, showTerminals }),
      ),
    ).toThrow("Invalid preferences");
});

it("defaults the hide-old threshold for previously saved preferences", () => {
  const { hideOldChatsThreshold: _, ...legacy } = defaultStudioPreferences;
  expect(
    parseStudioPreferences(JSON.stringify(legacy)).hideOldChatsThreshold,
  ).toBe(DEFAULT_HIDE_OLD_CHATS_THRESHOLD);
});

it("enables provider release checks for existing preference records", () => {
  const { checkProviderUpdates: _, ...legacy } = defaultStudioPreferences;
  expect(
    parseStudioPreferences(JSON.stringify(legacy)).checkProviderUpdates,
  ).toBe(DEFAULT_CHECK_PROVIDER_UPDATES);
  expect(
    parseStudioPreferences(
      JSON.stringify({
        ...defaultStudioPreferences,
        checkProviderUpdates: false,
      }),
    ).checkProviderUpdates,
  ).toBe(false);
  expect(() =>
    parseStudioPreferences(
      JSON.stringify({
        ...defaultStudioPreferences,
        checkProviderUpdates: "off",
      }),
    ),
  ).toThrow("Invalid preferences");
});

it("accepts only integer hide-old thresholds from 1 through 50", () => {
  for (const value of [1, 50])
    expect(
      parseStudioPreferences(
        JSON.stringify({
          ...defaultStudioPreferences,
          hideOldChatsThreshold: value,
        }),
      ).hideOldChatsThreshold,
    ).toBe(value);
  for (const value of [0, 51, 2.5, "3"])
    expect(() =>
      parseStudioPreferences(
        JSON.stringify({
          ...defaultStudioPreferences,
          hideOldChatsThreshold: value,
        }),
      ),
    ).toThrow("Invalid preferences");
});
