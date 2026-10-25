import { readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { expect, it } from "vitest";
import { nativeSignInMethods } from "./signInProtocol";

const repository = resolve(
  dirname(fileURLToPath(import.meta.url)),
  "../../../../../..",
);
const owner = join(
  repository,
  "workspaces/client/packages/client-core/src/desktop.rs",
);
const declaration =
  /pub const SIGN_IN_(PREPARE|OPEN|CANCEL)_METHOD: &str = "([^"]+)";/g;
const readOwner = (source: string) =>
  Object.fromEntries(
    [...source.matchAll(declaration)].map((match) => [
      match[1].toLowerCase(),
      match[2],
    ]),
  );
function files(directory: string): string[] {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? files(path) : [path];
  });
}

it("keeps every native sign-in method consumer on the desktop.rs contract", () => {
  const source = readFileSync(owner, "utf8");
  expect(readOwner(source)).toEqual(nativeSignInMethods);
  expect(
    readOwner(
      source.replace('"desktop.prepareSignIn"', '"desktop.changedSignIn"'),
    ),
  ).not.toEqual(nativeSignInMethods);
  const mirror = join(
    repository,
    "workspaces/client/apps/web/src/native/signInProtocol.ts",
  );
  for (const root of [
    "workspaces/client/apps/web/src",
    "workspaces/client/packages/client-core/src",
    "workspaces/client/apps/native/src-tauri/src",
  ]) {
    for (const path of files(join(repository, root))) {
      if (
        !/\.(?:[cm]?ts|tsx|mjs|rs)$/.test(path) ||
        /\.(?:test|stories)\./.test(path) ||
        path === mirror
      )
        continue;
      const text = readFileSync(path, "utf8");
      const consumer = path === owner ? text.replace(declaration, "") : text;
      expect(consumer, path).not.toMatch(
        /["']desktop\.(?:prepare|open|cancel)SignIn["']/,
      );
    }
  }
});
