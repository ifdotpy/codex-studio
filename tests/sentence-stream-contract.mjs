import assert from "node:assert/strict";
import { sentencePrefix } from "../web/src/components/sentenceStream.ts";
const cases = [
  ["Первое предложение. Второе ещё", "Первое предложение. "],
  [
    "First sentence. Second sentence! Unfinished",
    "First sentence. Second sentence! ",
  ],
  ["Value 3.14 remains unfinished", ""],
  ["Open https://example.com/file.md when", ""],
  [
    "See [a label. More](https://example.com). Next",
    "See [a label. More](https://example.com). ",
  ],
  ["Use `value. next` here. Pending", "Use `value. next` here. "],
  ["Finished\n\nNo end", "Finished\n\n"],
  ['Hello.\n\n```json\n{"a": 1}\n\n', "Hello.\n\n"],
  ['```json\n{"a": 1}\n```\nNext', '```json\n{"a": 1}\n```\n'],
  ["## Heading\nUnfinished", "## Heading\n"],
  ["- First item\n- Unfinished", "- First item\n"],
  ["No punctuation", ""],
  ["Value 3.", ""],
  ["Done. Use `unfinished. ", "Done. "],
  ["Done. See [unfinished. ", "Done. "],
  ["Done. **Unfinished. ", "Done. "],
  ["**First. Second.** Next", "**First. Second.** "],
];
for (const [input, expected] of cases)
  assert.equal(sentencePrefix(input, true), expected, input);
for (const [input] of cases)
  assert.equal(
    sentencePrefix(input, false),
    input,
    "Completion retains all source",
  );
console.log(
  "PASS sentence boundaries, Markdown, fences, decimal numbers, links, final tails",
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
