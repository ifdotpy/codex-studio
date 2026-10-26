import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { expect, test } from "vitest";
import type { Agent, Message } from "../../../types";
import TurnHistory from "./TurnHistory";
import {
  questionCard,
  questionHistoryIndex,
  transcriptRequests,
  type QuestionCardModel,
  type QuestionHistoryEntry,
  type QuestionLookup,
} from "../../questions/questionCardModel";
import { RequestCard } from "../../questions/Requests";
import type { components } from "../../../generated/api";

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

for (const afterTool of [false, true]) {
  test(`an empty async question source remains interactive ${afterTool ? "after a work row" : "as the first row"}`, () => {
    const source: Message = {
      id: "lead:empty-question",
      role: "assistant",
      text: "",
      turnId: "turn-1",
    };
    const history: QuestionHistoryEntry = {
      id: "request-empty",
      agent: "lead",
      itemId: "empty-question",
      method: "agent/asyncQuestion",
      status: "pending",
      questions: [
        { id: "0", question: "Continue with the release?", isSecret: false },
      ],
    };
    const request: components["schemas"]["RequestEntityDto"] = {
      id: history.id,
      agent: history.agent,
      method: history.method,
      status: "pending",
      params: { questions: history.questions },
    };
    const lookup: QuestionLookup = {
      agentId: "lead",
      history: questionHistoryIndex([history]),
      pending: new Map(),
      pendingIds: new Set([request.id]),
    };
    const cards = (item: Message) => questionCard(item, lookup);
    const items = afterTool ? [tool("before"), source] : [source];
    const inline = transcriptRequests(items, [request], cards);
    expect(inline.get(source.id)).toBe(request);
    const markup = renderToStaticMarkup(
      createElement(
        MantineProvider,
        null,
        createElement(TurnHistory, {
          items,
          enabled: false,
          storageKey: "empty-async-question",
          questionCards: cards,
          requestCards: new Map(
            [...inline].map(([messageId, entry]) => [
              messageId,
              entry &&
                createElement(RequestCard, {
                  request: entry,
                  messageId,
                  scope: "empty-async-question",
                  agents: [],
                  refresh: async () => {},
                  notify: () => {},
                }),
            ]),
          ),
          renderMessage: (item: Message) => createElement("p", null, item.text),
          onJump: () => {},
        }),
      ),
    );
    expect(markup.match(/data-request="request-empty"/g)).toHaveLength(1);
    expect(markup).toContain("Continue with the release?");
    expect(markup).toContain('data-answer="request-empty"');
    expect(markup).not.toContain('class="question-card"');
  });
}

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

for (const source of ["tool", "message"] as const) {
  test(`a pending ${source} question uses its existing interactive card in place`, () => {
    const card: QuestionCardModel = {
      source,
      state: "waiting",
      asynchronous: source === "message",
      requestId: "request-9",
      questions: [
        {
          id: "0",
          question: "Which file?",
          options: [],
          multiSelect: false,
          isSecret: false,
        },
      ],
    };
    const markup = renderToStaticMarkup(
      createElement(
        MantineProvider,
        null,
        createElement(TurnHistory, {
          items: [question],
          enabled: false,
          storageKey: "inline-question",
          questionCards: () => card,
          requestCards: new Map([
            [
              question.id,
              createElement(
                "section",
                { "data-request": "request-9" },
                "Existing answer form",
              ),
            ],
          ]),
          renderMessage: () => createElement("p", null, "Original message"),
          onJump: () => {},
        }),
      ),
    );
    expect(markup).toContain('data-request="request-9"');
    expect(markup.match(/Existing answer form/g)).toHaveLength(1);
    expect(markup).not.toContain('class="question-card"');
    expect(markup).not.toContain("Original message");
  });
}

test("a pending approval replaces its tool row and remains mounted outside the history window", () => {
  const approval = tool("approval");
  const markup = renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(TurnHistory, {
        items: [
          approval,
          ...Array.from({ length: 100 }, (_, index): Message => ({
            id: `message-${index}`,
            role: "user",
            text: "Later message",
          })),
        ],
        enabled: false,
        storageKey: "inline-approval",
        scrollContainer: { current: null },
        requestCards: new Map([
          [
            approval.id,
            createElement(
              "section",
              { "data-request": "approval-request" },
              "Existing approval actions",
            ),
          ],
        ]),
        renderMessage: (item: Message) => createElement("p", null, item.text),
        onJump: () => {},
      }),
    ),
  );
  expect(markup).toContain("data-history-window");
  expect(markup.match(/data-request="approval-request"/g)).toHaveLength(1);
  expect(markup).not.toContain("WebSearch");
});
