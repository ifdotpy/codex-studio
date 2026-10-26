import { expect, it } from "vitest";
import { combineBrowserProfiles } from "./combineBrowserProfiles.mjs";

const profile = (owner, preferences, databases = []) => ({
  owner,
  snapshot: { version: 2, preferences, databases },
});
const database = (name, sha256) => ({
  name,
  version: 1,
  stores: [{ name: "records", chunks: [{ sha256, bytes: 30, records: 1 }] }],
});

it("combines scoped drafts and databases without losing the chunk's source owner", () => {
  const source = [
    profile("root", { draft: "local" }, [database("local", "a")]),
    profile("server:remote", { ":server:remote:draft": "remote" }, [
      database("remote", "b"),
    ]),
  ];
  const before = structuredClone(source);
  const result = combineBrowserProfiles(source);
  expect(result.preferences).toEqual({
    draft: "local",
    ":server:remote:draft": "remote",
  });
  expect(result.databases.map((db) => db.stores[0].chunks[0].owner)).toEqual([
    "root",
    "server:remote",
  ]);
  expect(source).toEqual(before);
});
it("rejects conflicting preferences and databases before an importer can write", () => {
  expect(() =>
    combineBrowserProfiles([
      profile("root", { draft: "a" }),
      profile("server:local", { draft: "b" }),
    ]),
  ).toThrow("preference conflict");
  expect(() =>
    combineBrowserProfiles([
      profile("root", {}, [database("same", "a")]),
      profile("server:local", {}, [database("same", "b")]),
    ]),
  ).toThrow("database conflict");
});
it("deduplicates identical local/root data and preserves root-only receipt identity", () => {
  const root = profile("root", { draft: "a" }, [database("same", "a")]);
  expect(combineBrowserProfiles([root])).toBe(root.snapshot);
  const result = combineBrowserProfiles([
    root,
    { owner: "server:local", snapshot: structuredClone(root.snapshot) },
  ]);
  expect(result.databases).toHaveLength(1);
});
it("requires an explicit unique root and valid server mappings", () => {
  expect(() => combineBrowserProfiles([profile("server:remote", {})])).toThrow(
    "root owner",
  );
  expect(() =>
    combineBrowserProfiles([profile("root", {}), profile("root", {})]),
  ).toThrow("duplicate");
  expect(() =>
    combineBrowserProfiles([
      profile("root", {}),
      profile("server:../other", {}),
    ]),
  ).toThrow("Invalid");
});
