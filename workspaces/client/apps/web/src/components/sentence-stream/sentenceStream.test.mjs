import assert from "node:assert/strict";

import { it } from "vitest";

it("keeps streamed text visible and returns sentence offsets", async () => {
  const { sentencePrefix, sentences } = await import("./sentenceStream.ts");
  const cases = [
    "Первое предложение. Второе ещё",
    "First sentence. Second sentence! Unfinished",
    "Value 3.14 remains unfinished",
    "Open https://example.com/file.md when",
    "See [a label. More](https://example.com). Next",
    "Use `value. next` here. Pending",
    "Finished\n\nNo end",
    'Hello.\n\n```json\n{"a": 1}\n\n',
    '```json\n{"a": 1}\n```\nNext',
    "## Heading\nUnfinished",
    "- First item\n- Unfinished",
    "No punctuation",
    "Value 3.",
    "Done. Use `unfinished. ",
    "Done. See [unfinished. ",
    "Done. **Unfinished. ",
    "**First. Second.** Next",
  ];
  for (const input of cases)
    assert.equal(
      sentencePrefix(input, true),
      input,
      "Partial source stays visible: " + input,
    );
  for (const input of cases)
    assert.equal(
      sentencePrefix(input, false),
      input,
      "Completion retains all source",
    );
  console.log(
    "PASS immediate partial text, Markdown, fences, decimal numbers, links, final tails",
  );
  for (const input of [
    "A result costs 3.14 dollars. Next sentence. ",
    "See https://example.com/file.md for evidence. Continue. ",
    "Done. Use `value. next` here. Continue. ",
    "Done. **First. Second.** Next sentence. ",
    "Done. See [a label. More](https://example.com). Next. ",
  ]) {
    let previous = "";
    for (let size = 1; size <= input.length; size++) {
      const next = sentencePrefix(input.slice(0, size), true);
      assert.ok(
        next.startsWith(previous),
        `Stream must not retract: ${input.slice(0, size)}`,
      );
      previous = next;
    }
  }
  console.log("PASS character-by-character stream never retracts visible text");
  for (const [input, expected] of [
    [
      "Первое предложение. Второе!",
      [
        { text: "Первое предложение. ", index: 0 },
        { text: "Второе!", index: 20 },
      ],
    ],
    [
      "First sentence. Second!",
      [
        { text: "First sentence. ", index: 0 },
        { text: "Second!", index: 16 },
      ],
    ],
  ]) {
    assert.deepEqual(sentences(input), expected);
  }
});
