import { test } from "vitest";
import assert from "node:assert/strict";
import { inputGroups, sdkInputMessages } from "./input-messages.ts";
import { InputQueue } from "./features.mjs";

const convert = async (input) =>
  input.map(({ type, text }) => ({ type, text }));

test("unmarked input retains the legacy user message shape", async () => {
  const groups = await inputGroups([{ type: "text", text: "hello" }], convert);
  assert.deepEqual(
    sdkInputMessages("session", "input", groups, (id) => id),
    [
      {
        type: "user",
        uuid: "input",
        session_id: "session",
        parent_tool_use_id: null,
        message: { role: "user", content: [{ type: "text", text: "hello" }] },
      },
    ],
  );
});

test("unknown input origins fail before conversion", async () => {
  let converted = false;
  await assert.rejects(
    inputGroups([{ inputOrigin: "typo" }], async () => {
      converted = true;
    }),
    /origin/,
  );
  assert.equal(converted, false);
});

test("a split input leaves the retry queue before its first part is yielded", async () => {
  const messages = sdkInputMessages(
    "session",
    "input",
    await inputGroups(
      [
        { type: "text", text: "report", inputOrigin: "studio_event" },
        { type: "text", text: "human", inputOrigin: "human" },
      ],
      convert,
    ),
    (id) => id,
  );
  const wrapper = messages.at(-1);
  Object.defineProperty(wrapper, "studioInputBatch", { value: messages });
  const queue = new InputQueue();
  queue.push(wrapper);
  const iterator = queue[Symbol.asyncIterator]();
  assert.equal((await iterator.next()).value, messages[0]);
  assert.equal(queue.values.length, 0);
  assert.equal(queue.nonQueryIds.has("input:part:0"), true);
  assert.deepEqual(queue.withdraw(), [messages[1]]);
  assert.equal(queue.closed, false);
  queue.push({ uuid: "later" });
  assert.deepEqual((await iterator.next()).value, { uuid: "later" });
  queue.close();
  assert.equal((await iterator.next()).done, true);
});
