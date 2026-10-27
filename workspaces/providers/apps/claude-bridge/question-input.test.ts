import ts from "typescript";
import assert from "node:assert/strict";
import { test } from "vitest";
import { sdkDiagnostics } from "./testing/sdk-typecheck.ts";

test("an added SDK option field requires an explicit parser decision", () => {
  const errors = sdkDiagnostics(
    "question-input.ts",
    (source) => {
      assert.ok(source.includes("preview?: string;"));
      return source.replaceAll(
        "preview?: string;",
        "preview?: string; futureField?: string;",
      );
    },
    "tools",
  );
  assert.ok(
    errors.some((error) => error.file?.fileName.endsWith("/question-input.ts")),
  );
});

test("a changed required question field fails the actual bridge dialog caller", () => {
  const errors = sdkDiagnostics(
    "bridge.ts",
    (source) => {
      assert.ok(source.includes("multiSelect: boolean;"));
      return source.replaceAll("multiSelect: boolean;", "multiSelect: string;");
    },
    "tools",
  );
  assert.ok(
    errors.some((error) => error.file?.fileName.endsWith("/bridge.ts")),
    "A synthesized fresh dialog must satisfy the SDK tool owner",
  );
});

import type { AskUserQuestionInput } from "@anthropic-ai/claude-agent-sdk/sdk-tools";
import { parseUserQuestions } from "./question-input.ts";
import { startBridge } from "./testing/bridge-harness.ts";
const valid = [
  {
    question: "Proceed?",
    header: "Choice",
    multiSelect: false,
    options: [
      { label: "Yes", description: "Continue" },
      { label: "No", description: "Stop" },
    ],
  },
] satisfies AskUserQuestionInput["questions"];

for (const [name, input] of [
  ["non-array", {}],
  ["empty", []],
  ["too many questions", Array(5).fill(valid[0])],
  [
    "missing multiSelect",
    [{ question: "Proceed?", header: "Choice", options: valid[0].options }],
  ],
  ["malformed question", [{ ...valid[0], question: 3 }]],
  ["too few choices", [{ ...valid[0], options: valid[0].options.slice(0, 1) }]],
  [
    "too many choices",
    [{ ...valid[0], options: Array(5).fill(valid[0].options[0]) }],
  ],
  [
    "malformed choice",
    [{ ...valid[0], options: [{ label: "Yes" }, valid[0].options[1]] }],
  ],
] as const) {
  test(`malformed fresh ${name} is denied by the actual permission caller`, async () => {
    assert.equal(parseUserQuestions(input), null);
    const bridge = await startBridge(
      new URL("./question-input.fixture.mts", import.meta.url),
      "claude-invalid-question-",
      { env: { STUDIO_QUESTION_INPUT: JSON.stringify(input) } },
    );
    try {
      await bridge.call("turn/start", {
        threadId: bridge.thread,
        input: [{ type: "text", text: "Ask" }],
      });
      assert.equal(
        (await bridge.note("fixture/permission")).params.behavior,
        "deny",
      );
      assert.equal(
        bridge.notes.some(
          (note) => note.method === "item/tool/requestUserInput",
        ),
        false,
      );
    } finally {
      await bridge.close();
    }
  });
}

test("valid SDK questions reach the actual bridge user-input caller", async () => {
  assert.deepEqual(parseUserQuestions(valid), valid);
  const bridge = await startBridge(
    new URL("./question-input.fixture.mts", import.meta.url),
    "claude-valid-question-",
    { env: { STUDIO_QUESTION_INPUT: JSON.stringify(valid) } },
  );
  try {
    await bridge.call("turn/start", {
      threadId: bridge.thread,
      input: [{ type: "text", text: "Ask" }],
    });
    const request = await bridge.note("item/tool/requestUserInput");
    assert.equal(request.params.questions[0]?.multiSelect, false);
    bridge.answer(request.id, { answers: { "0": { answers: ["Yes"] } } });
    const response = (await bridge.note("fixture/permission")).params;
    assert.equal(response.behavior, "allow");
    if (response.behavior === "allow")
      assert.deepEqual(response.updatedInput?.answers, { "Proceed?": "Yes" });
  } finally {
    await bridge.close();
  }
});

test("an SDK question field mutation fails the constructed parser", () => {
  const errors = sdkDiagnostics(
    "question-input.ts",
    (source) =>
      source.replaceAll("multiSelect: boolean;", "multiSelect: string;"),
    "tools",
  );
  assert.ok(
    errors.some((error) => error.file?.fileName.endsWith("/question-input.ts")),
  );
});

test("an added SDK option cardinality fails the exact constructed parser", () => {
  const errors = sdkDiagnostics(
    "question-input.ts",
    (source) => {
      const ast = ts.createSourceFile(
        "sdk-tools.d.ts",
        source,
        ts.ScriptTarget.ESNext,
        true,
      );
      const owner = ast.statements.find(
        (node) =>
          ts.isInterfaceDeclaration(node) &&
          node.name.text === "AskUserQuestionInput",
      );
      assert.ok(owner);
      let field: ts.TypeNode | undefined;
      function visit(node: ts.Node) {
        if (
          !field &&
          ts.isPropertySignature(node) &&
          node.name.getText(ast) === "options"
        )
          field = node.type;
        ts.forEachChild(node, visit);
      }
      visit(owner);
      assert.ok(field);
      const start = field.getStart(ast),
        end = field.getEnd();
      return (
        source.slice(0, start) +
        "[] | (" +
        source.slice(start, end).trimStart().replace(/^\|/, "") +
        ")" +
        source.slice(end)
      );
    },
    "tools",
  );
  assert.ok(
    errors.some((error) => error.file?.fileName.endsWith("/question-input.ts")),
  );
});
