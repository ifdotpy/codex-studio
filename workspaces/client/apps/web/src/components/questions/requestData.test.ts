import { describe, expect, it } from "vitest";
import type { components } from "../../generated/api";
import { answerList } from "./AnswerFields";
import { requestApprovalDetails, requestQuestions } from "./requestData";

type RequestDto = components["schemas"]["RequestEntityDto"];

describe("requestQuestions", () => {
  it("preserves the producer's question fields and option descriptions", () => {
    const request: RequestDto = {
      id: "request-1",
      method: "agent/asyncQuestion",
      params: {
        questions: [
          {
            id: "0",
            question: "Choose formats",
            multiSelect: true,
            isSecret: false,
            options: [
              { label: "JSON", description: "Structured output" },
              { label: "Text" },
            ],
          },
          {
            id: "1",
            question: "API key",
            multiSelect: false,
            isSecret: true,
            options: [],
          },
        ],
      },
    };

    expect(requestQuestions(request)).toEqual([
      {
        id: "0",
        question: "Choose formats",
        multiSelect: true,
        isSecret: false,
        isOther: true,
        options: [
          { label: "JSON", description: "Structured output" },
          { label: "Text" },
        ],
      },
      {
        id: "1",
        question: "API key",
        multiSelect: false,
        isSecret: true,
        isOther: true,
        options: [],
      },
    ]);
  });

  it("reconstructs schema-backed single and multi-select questions", () => {
    const request: RequestDto = {
      id: "request-2",
      params: {
        requestedSchema: {
          properties: {
            color: { title: "Favorite color", enum: ["blue", "green"] },
            tags: {
              title: "Tags",
              type: "array",
              items: { enum: ["one", "two"] },
            },
            emptyTags: { type: "array", items: { enum: [] } },
            password: { format: "password", writeOnly: true },
          },
        },
      },
    };

    expect(requestQuestions(request)).toEqual([
      {
        id: "color",
        question: "Favorite color",
        options: [{ label: "blue" }, { label: "green" }],
        multiSelect: false,
        isSecret: false,
      },
      {
        id: "tags",
        question: "Tags",
        options: [{ label: "one" }, { label: "two" }],
        multiSelect: true,
        isSecret: false,
      },
      {
        id: "emptyTags",
        question: "emptyTags",
        options: [],
        multiSelect: true,
        isSecret: false,
      },
      {
        id: "password",
        question: "password",
        options: [],
        multiSelect: false,
        isSecret: true,
      },
    ]);
  });
});

describe("requestApprovalDetails", () => {
  it("falls back to command and changes from the produced top-level preview", () => {
    const request: RequestDto = {
      id: "approval-1",
      method: "item/commandExecution/requestApproval",
      params: { command: null, permissions: null },
      preview: {
        command: ["npm", "run", "build"],
        changes: [{ path: "src/app.ts", kind: "modify" }],
      },
    };

    expect(requestApprovalDetails(request)).toEqual({
      command: ["npm", "run", "build"],
      permissions: [{ path: "src/app.ts", kind: "modify" }],
    });
  });

  it("prefers explicit request command and permissions", () => {
    const request: RequestDto = {
      id: "approval-2",
      params: {
        command: ["git", "status"],
        permissions: { mode: "workspace-write" },
      },
      preview: {
        command: ["npm", "test"],
        changes: { files: ["change-preview"] },
      },
    };

    expect(requestApprovalDetails(request)).toEqual({
      command: ["git", "status"],
      permissions: { mode: "workspace-write" },
    });
  });
});

describe("custom answer permission", () => {
  it("honors native isOther and rejects stale custom enum drafts", () => {
    const request: RequestDto = {
      id: "native",
      method: "item/tool/requestUserInput",
      params: {
        questions: [
          { id: "closed", question: "Closed", options: [{ label: "One" }] },
          {
            id: "open",
            question: "Open",
            isOther: true,
            options: [{ label: "One" }],
          },
        ],
      },
    };
    const [closed, open] = requestQuestions(request);
    expect(closed.isOther).toBe(false);
    expect(open.isOther).toBe(true);
    expect(answerList(closed, { selected: ["One"], other: "Custom" })).toEqual([
      "One",
    ]);
    expect(answerList(open, { selected: [], other: "Custom" })).toEqual([
      "Custom",
    ]);
  });
});
