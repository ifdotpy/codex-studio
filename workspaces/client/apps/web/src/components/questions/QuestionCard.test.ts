import {
  approval,
  question as questionField,
  simpleRequest,
} from "../../approvalFixtures";
import type { Approval } from "../../approvals";
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
  transcriptRequests,
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

/** The state of a question that its owner has answered. */
function answeredState(
  answers: { id: string; answer: string[] }[],
): Approval["state"] {
  return {
    kind: "answered",
    answer: { kind: "answers", answers },
    by: { kind: "owner" },
    at: 2,
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
  it("keeps one editor when a tool and message refer to the same request", () => {
    const request = approval({
      id: "question",
      agent: AGENT,
      request: { kind: "question", questions: [questionField("0", "Which?")] },
    });
    const tool = askItem({ status: "inProgress" }, "running");
    const message: Message = {
      id: "lead:message",
      role: "assistant",
      text: "Which file?",
    };
    const card: ReturnType<typeof questionCard> = {
      source: "message",
      state: "waiting",
      asynchronous: true,
      questions: [],
      requestId: request.id,
    };
    const linked = transcriptRequests([tool, message], [request], () => card);
    expect(linked.get(tool.id)).toBe(request);
    expect(linked.get(message.id)).toBeNull();
    expect([...linked.values()].filter(Boolean)).toHaveLength(1);
  });
  it("links only loaded, active request rows by exact question or approval identity", () => {
    const question = askItem({ status: "inProgress" }, "running");
    const command: Message = {
      id: "lead:command",
      role: "output",
      text: "Command",
    };
    const exec = {
      kind: "exec",
      command: "pwd",
      cwd: "/",
      reason: null,
    } as const;
    const requests: Approval[] = [
      approval({
        id: "question",
        agent: AGENT,
        request: {
          kind: "question",
          questions: [questionField("0", "Which?")],
        },
        params: { itemId: "toolu_1" },
      }),
      approval({
        id: "approval",
        agent: AGENT,
        request: exec,
        params: { itemId: "command" },
      }),
      approval({
        id: "other-agent",
        agent: "worker",
        request: exec,
        params: { itemId: "command" },
      }),
      approval({
        id: "outside-page",
        agent: AGENT,
        request: exec,
        params: { itemId: "older" },
      }),
    ];
    const linked = transcriptRequests(
      [question, command],
      requests,
      () => undefined,
    );
    expect(
      [...linked].map(([message, request]) => [message, request?.id]),
    ).toEqual([
      [question.id, "question"],
      [command.id, "approval"],
    ]);
    expect(
      transcriptRequests(
        [command],
        requests.map((request) =>
          approval({
            id: request.id,
            agent: request.agent,
            request: request.request,
            params: request.detail.params,
            deferred: true,
          }),
        ),
        () => undefined,
      ).size,
    ).toBe(0);
    expect(
      transcriptRequests(
        [command],
        requests.map((request) =>
          simpleRequest({
            id: request.id,
            agent: request.agent,
            answered: true,
          }),
        ),
        () => undefined,
      ).size,
    ).toBe(0);
    const asynchronous: Message = {
      id: "lead:async",
      role: "assistant",
      text: "Continue?",
    };
    expect(
      transcriptRequests([asynchronous], requests, () => ({
        source: "message",
        state: "waiting",
        asynchronous: true,
        questions: [],
        requestId: "question",
      })).get(asynchronous.id)?.id,
    ).toBe("question");
  });
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
        approval({
          id: "request-1",
          agent: AGENT,
          request: {
            kind: "question",
            questions: [questionField("0", "Where should the release go?")],
          },
          params: { itemId: "toolu_1" },
          state: answeredState([{ id: "0", answer: ["Dev", "Prod, US"] }]),
        }),
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
    const entry: QuestionHistoryEntry = approval({
      id: `${AGENT}:question:call_7`,
      agent: AGENT,
      request: {
        kind: "agent_question",
        questions: [questionField("0", "Which scope?", ["One", "All"])],
      },
      params: { itemId: "call_7" },
    });
    const waiting = render(message, lookup([entry], [["", entry.id]]));
    expect(waiting).toContain("Agent can continue");
    expect(waiting).toContain(">Answer</");
    // The message already shows the question text.
    expect(waiting).not.toContain('class="question-card-text"');
    const answered = render(
      message,
      lookup([
        { ...entry, state: answeredState([{ id: "0", answer: ["All"] }]) },
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
