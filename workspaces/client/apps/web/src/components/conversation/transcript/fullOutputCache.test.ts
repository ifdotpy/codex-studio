import assert from "node:assert/strict";
import { beforeEach, it } from "vitest";
import { clearFullOutputCache, fullOutputText } from "./fullOutputCache";

beforeEach(clearFullOutputCache);

it("fetches a hash once and serves later opens from the cache", async () => {
  let calls = 0;
  const fetchText = async (hash: string) => {
    calls += 1;
    return `text of ${hash}`;
  };
  assert.equal(
    await fullOutputText("a".repeat(64), fetchText),
    `text of ${"a".repeat(64)}`,
  );
  assert.equal(
    await fullOutputText("a".repeat(64), fetchText),
    `text of ${"a".repeat(64)}`,
  );
  assert.equal(calls, 1);
  await fullOutputText("b".repeat(64), fetchText);
  assert.equal(calls, 2);
});

it("shares one request between simultaneous opens", async () => {
  let calls = 0;
  const fetchText = async () => {
    calls += 1;
    return "full";
  };
  const [first, second] = await Promise.all([
    fullOutputText("c".repeat(64), fetchText),
    fullOutputText("c".repeat(64), fetchText),
  ]);
  assert.equal(first, second);
  assert.equal(calls, 1);
});

it("does not keep a failed fetch", async () => {
  let calls = 0;
  const fetchText = async () => {
    calls += 1;
    if (calls === 1) throw new Error("offline");
    return "full";
  };
  await assert.rejects(fullOutputText("d".repeat(64), fetchText), /offline/);
  assert.equal(await fullOutputText("d".repeat(64), fetchText), "full");
  assert.equal(calls, 2);
});
