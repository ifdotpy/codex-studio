import { expect, test } from "vitest";
import {
  answered,
  changeRequest,
  listSecrets,
  MAX_VALUE_BYTES,
  nameProblem,
  removeSecret,
  setSecret,
  valueProblem,
} from "./ProjectSecrets";

test.each([
  ["ZULIP_APEIRO_SHOWROOM_API_KEY", null],
  ["_PRIVATE", null],
  ["A".repeat(64), null],
  ["", "Enter a name"],
  ["A".repeat(65), "Use at most 64 characters"],
  ["1KEY", "Start with a letter or _"],
  ["api_key", "Use only A to Z, 0 to 9 and _"],
  ["API-KEY", "Use only A to Z, 0 to 9 and _"],
  ["API KEY", "Use only A to Z, 0 to 9 and _"],
])("the name %j: %s", (name, problem) => {
  expect(nameProblem(name)).toBe(problem);
});

test("a value is not empty, has no NUL and has at most 16 KiB of UTF-8", () => {
  expect(valueProblem("secret")).toBeNull();
  expect(valueProblem("x".repeat(MAX_VALUE_BYTES))).toBeNull();
  expect(valueProblem("")).toBe("Enter a value");
  expect(valueProblem("a\0b")).toMatch(/NUL/);
  expect(valueProblem("x".repeat(MAX_VALUE_BYTES + 1))).toMatch(/16 KiB/);
  // Half as many two-byte characters demonstrates that the limit counts bytes.
  expect(valueProblem("é".repeat(MAX_VALUE_BYTES / 2 + 1))).toMatch(/16 KiB/);
});

test("the same change keeps its request ID until an answer; another change gets a new one", () => {
  let next = 0;
  const make = () => `id-${++next}`;
  const first = changeRequest(null, "set:TOKEN:3", make);
  expect(first).toEqual({ key: "set:TOKEN:3", requestId: "id-1" });
  // A retry after a lost reply sends the same ID; the VM returns its saved reply.
  expect(changeRequest(first, "set:TOKEN:3", make)).toBe(first);
  // An edit of the name or the value is another change.
  expect(changeRequest(first, "set:TOKEN:4", make).requestId).toBe("id-2");
  expect(changeRequest(first, "remove:TOKEN", make).requestId).toBe("id-3");
});

test("only an error with a server answer ends the change", () => {
  expect(answered(Object.assign(new Error("bad name"), { status: 400 }))).toBe(
    true,
  );
  expect(answered(Object.assign(new Error("VM down"), { status: 503 }))).toBe(
    true,
  );
  expect(answered(new TypeError("Failed to fetch"))).toBe(false);
  expect(answered(new Error("The server did not respond in time."))).toBe(
    false,
  );
  expect(answered("text")).toBe(false);
});

test("the calls send the value only to set and read names only", async () => {
  const calls: { path: string; body: unknown }[] = [];
  const backend = {
    post: (async (path: string, body: unknown) => {
      calls.push({ path, body });
      return { projectId: "p1", names: ["OTHER", "TOKEN"] };
    }) as never,
  };
  expect(await listSecrets(backend, "p1")).toEqual(["OTHER", "TOKEN"]);
  expect(await setSecret(backend, "p1", "TOKEN", "s3cret", "r-1")).toEqual([
    "OTHER",
    "TOKEN",
  ]);
  await removeSecret(backend, "p1", "OTHER", "r-2");
  expect(calls).toEqual([
    { path: "/api/project-secrets/list", body: { projectId: "p1" } },
    {
      path: "/api/project-secrets/set",
      body: {
        projectId: "p1",
        name: "TOKEN",
        value: "s3cret",
        requestId: "r-1",
      },
    },
    {
      path: "/api/project-secrets/remove",
      body: { projectId: "p1", name: "OTHER", requestId: "r-2" },
    },
  ]);
});
