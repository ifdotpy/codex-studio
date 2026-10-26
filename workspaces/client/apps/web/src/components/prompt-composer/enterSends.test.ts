import { describe, expect, it } from "vitest";
import { enterSends } from "./PromptInput";

const key = (fields: Partial<Parameters<typeof enterSends>[0]> = {}) => ({
  key: "Enter",
  shiftKey: false,
  metaKey: false,
  ctrlKey: false,
  isComposing: false,
  ...fields,
});

describe("composer Enter", () => {
  it("sends on a fine pointer and adds a line with Shift", () => {
    expect(enterSends(key(), false)).toBe(true);
    expect(enterSends(key({ shiftKey: true }), false)).toBe(false);
  });
  it("adds a line on a touch composer", () => {
    expect(enterSends(key(), true)).toBe(false);
    expect(enterSends(key({ shiftKey: true }), true)).toBe(false);
  });
  it("sends with Cmd or Ctrl on both pointer types", () => {
    for (const touch of [false, true]) {
      expect(enterSends(key({ metaKey: true }), touch)).toBe(true);
      expect(enterSends(key({ ctrlKey: true }), touch)).toBe(true);
    }
  });
  it("never sends during text composition or for other keys", () => {
    for (const touch of [false, true]) {
      expect(enterSends(key({ isComposing: true, metaKey: true }), touch)).toBe(
        false,
      );
      expect(enterSends(key({ key: "a", metaKey: true }), touch)).toBe(false);
    }
  });
});
