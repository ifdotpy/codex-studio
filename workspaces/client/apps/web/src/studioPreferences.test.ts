import { expect, it } from "vitest";
import {
  DEFAULT_CHECK_PROVIDER_UPDATES,
  DEFAULT_HIDE_OLD_CHATS_THRESHOLD,
  defaultStudioPreferences,
  parseStudioPreferences,
} from "./studioPreferences";

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
