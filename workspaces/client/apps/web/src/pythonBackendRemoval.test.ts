import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";

it("keeps web tests and scripts independent of the removed Python backend", () => {
  const root = fileURLToPath(new URL("../", import.meta.url));
  const files = ["package.json", "playwright-global-setup.ts"];
  for (const directory of ["tests", "scripts", "src"])
    files.push(
      ...readdirSync(join(root, directory), {
        recursive: true,
        encoding: "utf8",
      })
        .filter((file) => /\.(?:mjs|tsx?)$/.test(file))
        .map((file) => join(directory, file)),
    );
  const retiredBackend = ["runtime", "apps", "server"].join("/");
  const retiredInterpreter = new RegExp(
    [`["']`, "python", "3", `["']`].join(""),
  );
  const violations = files.filter((file) => {
    const source = readFileSync(join(root, file), "utf8");
    return source.includes(retiredBackend) || retiredInterpreter.test(source);
  });
  expect(violations).toEqual([]);
});
