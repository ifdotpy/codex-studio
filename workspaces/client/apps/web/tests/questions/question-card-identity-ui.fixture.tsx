import React, { useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import type { Message } from "../../src/types.ts";
import {
  approval,
  question as questionField,
} from "../../src/approvalFixtures.ts";
import type { Approval } from "../../src/approvals.ts";
import Requests, {
  RequestCard,
} from "../../src/components/questions/Requests.tsx";
import TurnHistory from "../../src/components/conversation/transcript/TurnHistory.tsx";
import {
  questionCard,
  transcriptRequests,
  type QuestionLookup,
} from "../../src/components/questions/questionCardModel.ts";

const question: Message = {
  id: "lead:question-item",
  role: "output",
  toolStatus: "running",
  text: JSON.stringify({
    id: "question-item",
    type: "mcpToolCall",
    tool: "AskUserQuestion",
    status: "inProgress",
    arguments: {
      questions: [{ question: "Continue?", options: [{ label: "Yes" }] }],
    },
  }),
};
const approval: Message = {
  id: "lead:command-item",
  role: "output",
  toolStatus: "running",
  text: JSON.stringify({
    id: "command-item",
    type: "commandExecution",
    command: "pwd",
    status: "inProgress",
  }),
};
const initial: Approval[] = [
  approval({
    id: "question",
    agent: "lead",
    request: {
      kind: "question",
      questions: [questionField("0", "Continue?", ["Yes"])],
    },
    params: { itemId: "question-item" },
  }),
  approval({
    id: "approval",
    agent: "lead",
    request: { kind: "exec", command: "pwd", cwd: "/", reason: null },
    params: { itemId: "command-item" },
  }),
];
function Fixture() {
  const [requests, setRequests] = useState(initial);
  const [items, setItems] = useState([question, approval]);
  const lookup: QuestionLookup = {
    agentId: "lead",
    history: new Map(),
    pending: new Map([["lead\u0000question-item", "question"]]),
    pendingIds: new Set(requests.map((request) => request.id)),
  };
  const cards = (item: Message) => questionCard(item, lookup);
  const linked = transcriptRequests(items, requests, cards);
  const ids = new Set(
    [...linked.values()].flatMap((request) => (request ? [request.id] : [])),
  );
  const notify = (text: string) => {
    throw new Error(text);
  };
  return (
    <main id="messages" style={{ maxWidth: 650, margin: "24px auto" }}>
      <TurnHistory
        items={items}
        enabled={false}
        storageKey="question-identity"
        questionCards={cards}
        requestCards={
          new Map(
            [...linked].map(([messageId, request]) => [
              messageId,
              request && (
                <RequestCard
                  key={request.id}
                  messageId={messageId}
                  request={request}
                  scope="identity"
                  agents={[]}
                  notify={notify}
                  refresh={async () => {
                    setRequests((current) =>
                      current.filter((entry) => entry.id !== request.id),
                    );
                    if (request.id === "question")
                      setItems((current) =>
                        current.map((item) =>
                          item.id === question.id
                            ? {
                                ...question,
                                toolStatus: "completed",
                                text: JSON.stringify({
                                  id: "question-item",
                                  type: "mcpToolCall",
                                  tool: "AskUserQuestion",
                                  status: "completed",
                                  arguments: {
                                    questions: [
                                      {
                                        question: "Continue?",
                                        options: [{ label: "Yes" }],
                                      },
                                    ],
                                  },
                                  result: {
                                    content:
                                      'User has answered: "Continue?"="Yes". Continue.',
                                  },
                                }),
                              }
                            : item,
                        ),
                      );
                  }}
                />
              ),
            ]),
          )
        }
        renderMessage={(item) => <p>{item.text}</p>}
        onJump={() => {}}
      />
      <Requests
        requests={requests.filter((request) => !ids.has(request.id))}
        allRequests={requests}
        scope="identity"
        agents={[]}
        notify={notify}
        refresh={async () => {}}
      />
    </main>
  );
}
const root = document.getElementById("root");
if (!root) throw new Error("Missing fixture root");
createRoot(root).render(
  <MantineProvider>
    <Fixture />
  </MantineProvider>,
);
