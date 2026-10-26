import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { MantineProvider } from "@mantine/core";
import { describe, expect, it } from "vitest";
import type { Message } from "../../types";
import contract from "../../../../../contracts/agent-question-item.json";
import QuestionCard from "./QuestionCard";
import {
  isQuestionToolItem,
  parseToolAnswers,
  questionCard,
  questionHistoryIndex,
  splitJoinedAnswer,
  type QuestionHistoryEntry,
  type QuestionLookup,
} from "./questionCardModel";

const AGENT = "lead";
const options = [
  { label: "Dev", description: "Deploy to the test cluster first" },
  { label: "Prod, EU", description: "Deploy to the EU production cluster" },
  { label: "Prod, US" },
];

// The shape both backends store for a Claude AskUserQuestion call: the
// bridge's mcpToolCall item, serialized as the transcript row text.
function askItem(
  fields: Record<string, unknown>,
  toolStatus?: string,
  multiSelect = false,
): Message {
  return {
    id: `${AGENT}:toolu_1`,
    role: "output",
    title: "mcpToolCall",
    toolStatus,
    text: JSON.stringify({
      id: "toolu_1",
      status: "completed",
      tool: "AskUserQuestion",
      type: "mcpToolCall",
      server: "claude",
      arguments: {
        questions: [
          {
            question: "Where should the release go?",
            header: "Deploy",
            multiSelect,
            options,
          },
        ],
      },
      ...fields,
    }),
  };
}

function lookup(
  history: QuestionHistoryEntry[] = [],
  pending: [string, string][] = [],
): QuestionLookup {
  return {
    agentId: AGENT,
    history: questionHistoryIndex(history),
    pending: new Map(
      pending.map(([item, id]) => [`${AGENT}\u0000${item}`, id]),
    ),
    pendingIds: new Set(pending.map(([, id]) => id)),
  };
}

function render(item: Message, context = lookup()) {
  const card = questionCard(item, context);
  if (!card) throw new Error("Expected a question card");
  return renderToStaticMarkup(
    createElement(
      MantineProvider,
      null,
      createElement(QuestionCard, {
        card,
        messageId: item.id,
        messageText: card.source === "message" ? item.text : undefined,
        onAnswer: () => {},
      }),
    ),
  );
}

const chosen = (markup: string) =>
  [
    ...markup.matchAll(
      /data-chosen="true"[\s\S]*?class="question-card-label">([^<]*)</g,
    ),
  ].map((match) => match[1]);

describe("question card", () => {
  it("shows a pending question as waiting with the existing answer action", () => {
    const item = askItem({ status: "inProgress" }, "running");
    const markup = render(item, lookup([], [["toolu_1", "request-1"]]));
    expect(
      questionCard(item, lookup([], [["toolu_1", "request-1"]])),
    ).toMatchObject({ state: "waiting", requestId: "request-1" });
    expect(markup).toContain("Waiting for your answer");
    expect(markup).toContain('class="question-card-header">Deploy<');
    expect(markup).toContain("Where should the release go?");
    expect(markup).toContain("Deploy to the EU production cluster");
    expect(markup).toContain(">Answer</");
    expect(chosen(markup)).toEqual([]);
  });

  it("marks the single chosen option from the stored Claude result", () => {
    const markup = render(
      askItem(
        {
          result: {
            content:
              'User has answered your questions: "Where should the release go?"="Dev". You can now continue with the user\'s answers in mind.',
          },
        },
        "completed",
      ),
    );
    expect(markup).toContain("Answered");
    expect(chosen(markup)).toEqual(["Dev"]);
    expect(markup).not.toContain("Multiple choice");
    expect(markup).not.toContain(">Answer</");
  });

  it("marks every chosen option of a multiple-choice question", () => {
    const item = askItem(
      {
        result: {
          content:
            'The user answered: "Where should the release go?"="Prod, EU, Prod, US". Read the answers carefully.',
        },
      },
      "completed",
      true,
    );
    const markup = render(item);
    expect(markup).toContain("Multiple choice");
    expect(chosen(markup)).toEqual(["Prod, EU", "Prod, US"]);
    // Exact answers from the question history win over the joined text.
    const exact = render(
      item,
      lookup([
        {
          id: "request-1",
          agent: AGENT,
          method: "item/tool/requestUserInput",
          status: "answered",
          itemId: "toolu_1",
          questions: [
            {
              id: "0",
              question: "Where should the release go?",
              isSecret: false,
            },
          ],
          answerHistory: [
            {
              id: "0",
              question: "Where should the release go?",
              isSecret: false,
              answer: ["Dev", "Prod, US"],
            },
          ],
        },
      ]),
    );
    expect(chosen(exact)).toEqual(["Dev", "Prod, US"]);
  });

  it("shows a free-text answer as the user's own answer", () => {
    const markup = render(
      askItem(
        {
          result: {
            content:
              'Your questions have been answered: "Where should the release go?"="Nowhere yet, "wait" for QA". You can now continue with these answers in mind.',
          },
        },
        "completed",
      ),
    );
    expect(chosen(markup)).toEqual([]);
    expect(markup).toContain("Own answer");
    expect(markup).toContain("Nowhere yet, &quot;wait&quot; for QA");
  });

  it("reads the stored item contract that both backends keep", () => {
    // Python and Rust tests store this same provider item unchanged.
    const markup = render({
      id: `${AGENT}:${contract.item.id}`,
      role: "output",
      title: "mcpToolCall",
      toolStatus: "completed",
      text: JSON.stringify(contract.item),
    });
    expect(markup).toContain('class="question-card-header">Deploy<');
    expect(markup).toContain("Multiple choice");
    expect(chosen(markup)).toEqual(["Dev", "Prod, EU"]);
  });

  it("keeps the generic tool view for malformed or clipped items", () => {
    const clipped: Message = {
      id: `${AGENT}:toolu_2`,
      role: "output",
      title: "mcpToolCall",
      text: '{"id": "toolu_2", "tool": "AskUserQuestion", "arguments": {"questions": [',
      truncated: true,
    };
    const noQuestions = askItem({ arguments: { questions: [] } });
    const badQuestion = askItem({
      arguments: { questions: [{ header: "No question text" }] },
    });
    const otherTool = askItem({ tool: "Read" });
    for (const item of [clipped, noQuestions, badQuestion, otherTool]) {
      expect(isQuestionToolItem(item)).toBe(false);
      expect(questionCard(item, lookup())).toBeUndefined();
    }
  });

  it("shows a failed question with its error", () => {
    const markup = render(
      askItem(
        {
          status: "failed",
          result: { content: "Only the orchestrator can ask the user." },
        },
        "failed",
      ),
    );
    expect(markup).toContain("Question failed");
    expect(markup).toContain("Only the orchestrator can ask the user.");
  });

  it("does not call a closed question answered", () => {
    const markup = render(
      askItem(
        { result: { content: "The user did not answer the questions." } },
        "completed",
      ),
    );
    expect(markup).toContain("Closed");
    expect(markup).not.toContain(">Answered<");
    expect(markup).toContain("The user did not answer the questions.");
  });

  it("renders a Codex asynchronous question from its history row", () => {
    const message: Message = {
      id: `${AGENT}:call_7`,
      role: "assistant",
      text: "Which scope?",
      phase: "final_answer",
    };
    const entry: QuestionHistoryEntry = {
      id: `${AGENT}:question:call_7`,
      agent: AGENT,
      method: "agent/asyncQuestion",
      status: "pending",
      itemId: "call_7",
      questions: [
        {
          id: "0",
          question: "Which scope?",
          isSecret: false,
          header: null,
          options: [
            { label: "One", description: null },
            { label: "All", description: null },
          ],
          multiSelect: false,
          isOther: false,
        },
      ],
    };
    const waiting = render(message, lookup([entry], [["", entry.id]]));
    expect(waiting).toContain("Agent can continue");
    expect(waiting).toContain(">Answer</");
    // The message already shows the question text.
    expect(waiting).not.toContain('class="question-card-text"');
    const answered = render(
      message,
      lookup([
        {
          ...entry,
          status: "answered",
          answerHistory: [
            {
              id: "0",
              question: "Which scope?",
              isSecret: false,
              answer: ["All"],
            },
          ],
        },
      ]),
    );
    expect(chosen(answered)).toEqual(["All"]);
    expect(
      questionCard({ ...message, id: `${AGENT}:other` }, lookup([entry])),
    ).toBeUndefined();
  });
});

describe("Claude answer text", () => {
  it("splits answers of several questions by their known question text", () => {
    expect(
      parseToolAnswers(
        'The user answered: "A?"="x", "B, c?"="y, z". Read the answers carefully — and follow them.',
        ["A?", "B, c?"],
      ),
    ).toEqual(["x", "y, z"]);
    expect(parseToolAnswers("Something else", ["A?"])).toBeUndefined();
  });

  it("rebuilds joined choices from option labels", () => {
    const question = { options, multiSelect: true };
    expect(splitJoinedAnswer("Prod, EU, Dev", question)).toEqual([
      "Prod, EU",
      "Dev",
    ]);
    expect(splitJoinedAnswer("Dev, later maybe", question)).toEqual([
      "Dev",
      "later maybe",
    ]);
    expect(
      splitJoinedAnswer("Dev, x", { options, multiSelect: false }),
    ).toEqual(["Dev, x"]);
  });
});
