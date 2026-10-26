import { cssFiles } from "./cssFixtureFiles";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";

const sourceRoot = fileURLToPath(new URL(".", import.meta.url));
const read = (path: string) => readFileSync(join(sourceRoot, path), "utf8");

it("shares the main-area scrollbar palette with native panes and Mantine controls", () => {
  const css = read("style.css");
  expect(css).toMatch(
    /--studio-scrollbar-thumb:\s*light-dark\(#18244718, #ffffff18\)/,
  );
  expect(css).toMatch(
    /\*\s*\{[^}]*scrollbar-width:\s*thin;[^}]*scrollbar-color:\s*var\(--studio-scrollbar-thumb\) transparent;/,
  );
  expect(css).toMatch(
    /\.mantine-ScrollArea-thumb[^{}]*\{[^}]*background(?:-color)?:\s*var\(--studio-scrollbar-thumb\)/,
  );
  expect(css).toMatch(
    /\.mantine-ScrollArea-scrollbar[^{}]*\{[^}]*background(?:-color)?:\s*transparent/,
  );
});

it("keeps palette ownership global and preserves native scrolling and Mantine viewport hiding", () => {
  const declarations = cssFiles(sourceRoot).flatMap((path) =>
    [
      ...readFileSync(path, "utf8").matchAll(/scrollbar-color:\s*([^;]+);/g),
    ].map((match) => match[1]),
  );
  expect(declarations).toEqual(["var(--studio-scrollbar-thumb) transparent"]);
  expect(read("style.css")).toMatch(/#messages\s*\{[^}]*overflow:\s*auto/);
  expect(read("components/sidebar-projects.css")).toMatch(
    /\.chat-scroll\s*\{[^}]*overflow-y:\s*auto;[^}]*overflow-x:\s*hidden;[^}]*overscroll-behavior:\s*contain/,
  );
  // Mantine must keep its hidden native viewport scrollbar: never override it globally with !important.
  expect(read("style.css")).not.toMatch(/scrollbar-width:[^;]*!important/);
});
