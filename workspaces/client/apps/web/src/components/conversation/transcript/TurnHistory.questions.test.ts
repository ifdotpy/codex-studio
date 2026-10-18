import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import type { Agent, Message } from "../../../types";
import TurnHistory from "./TurnHistory";

const question: Message = {
  id: "lead:toolu_9",
  role: "output",
  title: "mcpToolCall",
  turnId: "turn-1",
  turnStatus: "completed",
  toolStatus: "completed",
  text: JSON.stringify({
    id: "toolu_9",
    status: "completed",
    tool: "AskUserQuestion",
    type: "mcpToolCall",
    server: "claude",
    arguments: {
      questions: [
        {
          question: "Which file?",
          header: "File",
          multiSelect: false,
          options: [{ label: "a.ts" }, { label: "b.ts" }],
        },
      ],
    },
    result: {
      content:
        'User has answered your questions: "Which file?"="b.ts". You can now continue.',
    },
  }),
};

const tool = (id: string): Message => ({
  id: `lead:${id}`,
  role: "output",
  title: "mcpToolCall",
  turnId: "turn-1",
  turnStatus: "completed",
  toolStatus: "completed",
  text: JSON.stringify({
    type: "mcpToolCall",
    id,
    tool: "WebSearch",
    server: "claude",
    status: "completed",
  }),
});

test("a question renders as its own card between work logs, not as a tool row", () => {
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(TurnHistory, {
        items: [tool("before"), question, tool("after")],
        enabled: false,
        storageKey: "question-card-test",
        renderMessage: (item: Message) => createElement("p", null, item.text),
        onJump: () => {},
      }),
    ),
  );
  const card = markup.indexOf('class="question-card"');
  expect(card).toBeGreaterThan(0);
  expect(markup).toContain('data-chosen="true"');
  expect(markup).not.toContain("AskUserQuestion");
  // The tools on both sides stay in their own activity groups.
  expect(markup.slice(0, card)).toContain("tool-group");
  expect(markup.slice(card)).toContain("tool-group");
});

test("a managed turn keeps the question card outside its work log", () => {
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(TurnHistory, {
        items: [tool("before"), question, tool("after")],
        enabled: true,
        agent: { id: "lead", threadId: "thread", status: "idle" } as Agent,
        agentId: "lead",
        storageKey: "question-card-managed-test",
        renderMessage: (item: Message) => createElement("p", null, item.text),
        onJump: () => {},
      }),
    ),
  );
  const card = markup.indexOf('class="question-card"');
  expect(card).toBeGreaterThan(0);
  expect(markup.slice(0, card)).toContain("turn-work");
  expect(markup.slice(card)).toContain("turn-work");
});
