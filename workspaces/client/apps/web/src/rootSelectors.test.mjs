import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { it } from "vitest";

function cssFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    if (entry.isDirectory()) return cssFiles(path);
    return entry.name.endsWith(".css") ? [path] : [];
  });
}

// `#root:has(...)` makes the browser test the whole app tree after each DOM
// change. The owning components set data attributes on <html> instead.
it("has no :has() selector on the app root", () => {
  const found = cssFiles(new URL(".", import.meta.url).pathname).flatMap(
    (path) =>
      readFileSync(path, "utf8")
        .replace(/\/\*[\s\S]*?\*\//g, (comment) =>
          comment.replace(/[^\n]/g, ""),
        )
        .split("\n")
        .flatMap((line, index) =>
          /#root\s*:has\(/.test(line)
            ? [`${path.split("/src/").at(-1)}:${index + 1}`]
            : [],
        ),
  );
  assert.deepEqual(found, []);
});
