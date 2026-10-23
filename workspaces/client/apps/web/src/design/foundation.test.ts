import { readFileSync } from "node:fs";
import { expect, test } from "vitest";

const css = (path: string) =>
  readFileSync(new URL(path, import.meta.url), "utf8");

test("content width preference is the only owner of the custom transcript column", () => {
  expect(css("../style.css").includes("--studio-chat-max")).toBe(false);
  const preferences = css("../studio-preferences.css");
  expect(preferences).toContain("--studio-content-width-ratio");
  expect(preferences).toContain("--studio-content-column");
  expect(preferences).toContain('data-studio-content-layout="original"');
});
