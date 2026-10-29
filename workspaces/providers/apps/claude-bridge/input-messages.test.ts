import { test } from "vitest";
import assert from "node:assert/strict";
import {
  inputGroups,
  sdkInputMessages,
  type InputGroup,
} from "./input-messages.ts";
import { InputQueue } from "./features.ts";

const convert = async (
  input: { type: "text"; text: string; inputOrigin?: string }[],
): Promise<InputGroup["blocks"]> =>
  input.map(({ type, text }) => ({ type, text }));

test("unmarked input retains the legacy user message shape", async () => {
  const groups = await inputGroups([{ type: "text", text: "hello" }], convert);
  assert.deepEqual(
    sdkInputMessages(
      "session",
      "input",
      groups,
      (id) => id as ReturnType<typeof sdkInputMessages>[number]["uuid"],
    ),
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

// Claude CLI marks a task-notification message as "NOT USER INPUT". When it
// followed the human text, the model applied that mark to the human message.
test("the human text is the last message, after the Studio event", async () => {
  const messages = sdkInputMessages(
    "session",
    "input",
    await inputGroups(
      [
        { type: "text", text: "question", inputOrigin: "human" },
        { type: "text", text: "receipt time", inputOrigin: "studio_event" },
      ],
      convert,
    ),
    // @ts-expect-error The identity callback deliberately bypasses native UUID encoding.
    (id) => id,
  );
  assert.deepEqual(
    messages.map((message) => [
      message.uuid,
      message.message.content,
      "origin" in message,
      message.shouldQuery,
    ]),
    [
      ["input:part:0", [{ type: "text", text: "receipt time" }], true, false],
      ["input", [{ type: "text", text: "question" }], false, undefined],
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
    // @ts-expect-error The identity callback deliberately bypasses native UUID encoding.
    (id) => id,
  );
  const wrapper = messages.at(-1);
  Object.defineProperty(wrapper, "studioInputBatch", { value: messages });
  const queue = new InputQueue<{ uuid: string; shouldQuery?: boolean }>();
  queue.push(wrapper!);
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
