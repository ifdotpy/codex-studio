import { cssFiles } from "./cssFixtureFiles";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { it } from "vitest";

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
