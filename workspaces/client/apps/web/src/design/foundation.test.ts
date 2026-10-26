import { readFileSync } from "node:fs";
import { assert, expect, test } from "vitest";

const css = (path: string) =>
  readFileSync(new URL(path, import.meta.url), "utf8");

test("content width preference is the only owner of the custom transcript column", () => {
  expect(css("../style.css").includes("--studio-chat-max")).toBe(false);
  const preferences = css("../studio-preferences.css");
  expect(preferences).toContain("--studio-content-width-ratio");
  expect(preferences).toContain("--studio-content-column");
  expect(preferences).toContain('data-studio-content-layout="original"');
});

test("touch sizing uses Mantine variables without stretching NumberInput steppers", () => {
  const appearance = css("../appearance.css");
  expect(/min-(?:width|height):\s*44px\s*!important/.test(appearance)).toBe(
    false,
  );
  expect(appearance.includes("@media (pointer: coarse)")).toBe(true);
  expect(appearance.includes("--input-height:")).toBe(true);
  expect(appearance.includes("--button-height:")).toBe(true);
});

test("voice surfaces follow the active scheme instead of hard-coded dark colors", () => {
  const voice = css("../components/realtime-voice.css");
  expect(/#[\da-f]{3,8}\b|--mantine-color-dark-/i.test(voice)).toBe(false);
  expect(voice.includes("var(--surface)")).toBe(true);
  expect(voice.includes("var(--text)")).toBe(true);
});

test("the composer and submit action consume the shared surface and primary tokens", () => {
  const source = css("../style.css");
  for (const selector of ["#composer", "#send", "#send:disabled"]) {
    const section = source.split(`${selector} {`)[1];
    assert(section !== undefined);
    const block = section.split("}")[0];
    assert(block !== undefined);
    expect(/#[\da-f]{3,8}\b/i.test(block), selector).toBe(false);
  }
});

test("navigation links inherit the neutral text color in both schemes", () => {
  const source = css("../style.css");
  const section = source.split("\na {")[1];
  assert(section !== undefined);
  const links = section.split("}")[0];
  assert(links !== undefined);
  expect(links.includes("color: inherit")).toBe(true);
  expect(css("../appearance.css").includes('color-scheme="light"] a {')).toBe(
    false,
  );
});
