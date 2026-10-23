import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { it } from "vitest";

// An infinite animation runs on every frame for as long as it is shown. With
// opacity and transform only, the compositor runs it without a style update
// and a repaint of the page on each frame.
const compositorProperties = new Set(["opacity", "transform"]);

function cssFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return cssFiles(path);
    return entry.name.endsWith(".css") ? [path] : [];
  });
}

function keyframeProperties(css) {
  const blocks = new Map();
  const start = /@keyframes\s+([\w-]+)\s*\{/g;
  for (let match; (match = start.exec(css));) {
    let depth = 1;
    let index = start.lastIndex;
    while (depth && index < css.length) {
      if (css[index] === "{") depth++;
      else if (css[index] === "}") depth--;
      index++;
    }
    const body = css.slice(start.lastIndex, index - 1);
    const properties = new Set(
      [...body.matchAll(/([a-z-]+)\s*:/g)].map(([, name]) => name),
    );
    blocks.set(match[1], properties);
  }
  return blocks;
}

it("runs every infinite animation on compositor properties only", () => {
  const files = cssFiles(new URL(".", import.meta.url).pathname).map((path) =>
    readFileSync(path, "utf8").replace(/\/\*[\s\S]*?\*\//g, ""),
  );
  const keyframes = new Map(
    files.flatMap((css) => [...keyframeProperties(css)]),
  );
  const infinite = new Set(
    files.flatMap((css) =>
      [...css.matchAll(/animation\s*:\s*([\w-]+)[^;}]*\binfinite\b/g)].map(
        ([, name]) => name,
      ),
    ),
  );
  assert.ok(infinite.size > 0);
  for (const name of infinite) {
    const properties = keyframes.get(name);
    assert.ok(properties, `@keyframes ${name} is missing`);
    assert.deepEqual(
      [...properties].filter((property) => !compositorProperties.has(property)),
      [],
      `@keyframes ${name} animates properties outside the compositor`,
    );
  }
});

it("pauses inactive animations of pseudo-elements too", () => {
  const css = readFileSync(
    new URL("./visual-activity.css", import.meta.url),
    "utf8",
  );
  const selectors = css
    .slice(0, css.indexOf("{"))
    .split(",")
    .map((selector) => selector.trim().replace(/\s+/g, " "));
  // A universal selector does not match ::before or ::after.
  for (const scope of [
    '[data-visual-active="false"]',
    "details:not([open]) > :not(summary)",
  ])
    for (const pseudo of ["::before", "::after"])
      for (const selector of [`${scope}${pseudo}`, `${scope} *${pseudo}`])
        assert.ok(selectors.includes(selector), `${selector} is not paused`);
});
