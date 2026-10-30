import { approval, question } from "../../approvalFixtures";
import { required } from "../componentTestFixtures.ts";
import { describe, expect, it } from "vitest";
import { answerList } from "./AnswerFields";
import {
  browserOpenDetails,
  requestApprovalDetails,
  requestQuestions,
} from "./requestData";

const browserRequest = (
  url: string,
  params: Record<string, unknown>,
  id = "request-browser",
) => approval({ id, request: { kind: "browser_open", url }, params });

describe("requestQuestions", () => {
  it("preserves the producer's question fields and option descriptions", () => {
    const request = approval({
      id: "request-1",
      request: {
        kind: "agent_question",
        questions: [
          {
            ...question("0", "Choose formats"),
            multi_select: true,
            options: [
              {
                label: "JSON",
                description: "Structured output",
                preview: null,
              },
              { label: "Text", description: null, preview: null },
            ],
          },
          { ...question("1", "API key"), multi_select: false, is_secret: true },
        ],
      },
    });

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
    const request = approval({
      id: "request-2",
      request: {
        kind: "elicitation",
        schema: {
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
      params: { mode: "form" },
    });

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

  it("shows no fields for a link elicitation or an approval", () => {
    expect(
      requestQuestions(
        approval({
          request: { kind: "elicitation", schema: null },
          params: { mode: "url", url: "https://example.com" },
        }),
      ),
    ).toEqual([]);
    expect(
      requestQuestions(
        approval({
          request: { kind: "exec", command: "pwd", cwd: "/", reason: null },
        }),
      ),
    ).toEqual([]);
  });
});

describe("requestApprovalDetails", () => {
  it("reads the command, folder and reason of an exec request", () => {
    const request = approval({
      request: {
        kind: "exec",
        command: "npm run build",
        cwd: "/work",
        reason: "Build the app",
      },
    });
    expect(requestApprovalDetails(request)).toMatchObject({
      command: "npm run build",
      cwd: "/work",
      reason: "Build the app",
    });
  });

  it("lists the files of a patch and the monitor command of a start", () => {
    expect(
      requestApprovalDetails(
        approval({
          request: {
            kind: "patch",
            item: "item-1",
            files: ["src/app.ts"],
            reason: null,
          },
        }),
      ),
    ).toMatchObject({ files: ["src/app.ts"] });
    expect(
      requestApprovalDetails(
        approval({
          request: { kind: "monitor_start", monitor: "m1", command: "tail" },
        }),
      ),
    ).toMatchObject({ command: "tail" });
  });

  it("prefers the provider's permission details over the requested names", () => {
    const names = approval({
      request: { kind: "permissions", requested: ["network"] },
    });
    expect(requestApprovalDetails(names).permissions).toEqual(["network"]);
    const details = approval({
      request: { kind: "permissions", requested: ["fileSystem"] },
      params: { permissions: { fileSystem: { write: ["/work"] } } },
    });
    expect(requestApprovalDetails(details).permissions).toEqual({
      fileSystem: { write: ["/work"] },
    });
  });

  it("names the tool of a tool request and the link of an elicitation", () => {
    expect(
      requestApprovalDetails(
        approval({ request: { kind: "tool_use", tool: "Bash", input: {} } }),
      ).tool,
    ).toBe("Bash");
    expect(
      requestApprovalDetails(
        approval({
          request: { kind: "elicitation", schema: null },
          params: { mode: "url", url: "https://example.com" },
        }),
      ).url,
    ).toBe("https://example.com");
  });
});

describe("custom answer permission", () => {
  it("honors native isOther and rejects stale custom enum drafts", () => {
    const request = approval({
      id: "native",
      request: {
        kind: "question",
        questions: [
          question("closed", "Closed", ["One"]),
          { ...question("open", "Open", ["One"]), is_other: true },
        ],
      },
    });
    const [closed, open] = requestQuestions(request);

    expect(required(closed).isOther).toBe(false);
    expect(required(open).isOther).toBe(true);
    expect(
      answerList(required(closed), { selected: ["One"], other: "Custom" }),
    ).toEqual(["One"]);
    expect(
      answerList(required(open), { selected: [], other: "Custom" }),
    ).toEqual(["Custom"]);
  });
});

describe("browserOpenDetails", () => {
  it("reads the sign-in host, the forward port and the OAuth client", () => {
    const request = browserRequest(
      "https://accounts.example.com/authorize?client_id=kube",
      {
        bridgeId: "b1",
        host: "accounts.example.com",
        redirect: { host: "localhost", port: 8000, path: "/" },
        clientId: "kube",
        scope: "openid",
        linuxUser: "studio-a-1",
      },
    );
    expect(browserOpenDetails(request)).toEqual({
      host: "accounts.example.com",
      url: "https://accounts.example.com/authorize?client_id=kube",
      port: 8000,
      redirectPath: "/",
      clientId: "kube",
      scope: "openid",
      linuxUser: "studio-a-1",
    });
  });

  it("reads the waiting phase, its deadline, and a sent paste", () => {
    const params = {
      host: "id.example.com",
      redirect: { host: "127.0.0.1", port: 40645, path: "/oauth/callback" },
      expiresAt: 1_800_000_600,
      phase: "waiting",
      waiting: { expiresAt: 1_800_000_900 },
      paste: { at: 1_800_000_100 },
      outcome: null,
    };
    const url = "https://id.example.com/authorize?state=s";
    expect(browserOpenDetails(browserRequest(url, params))).toMatchObject({
      port: 40645,
      redirectPath: "/oauth/callback",
      phase: "waiting",
      expiresAt: 1_800_000_600,
      waitingExpiresAt: 1_800_000_900,
      pasteSent: true,
    });
    const before = browserOpenDetails(
      browserRequest(url, { ...params, phase: "other", paste: null }),
    );
    expect(before?.phase).toBeUndefined();
    expect(before?.pasteSent).toBeUndefined();
  });

  it("has no forward port for a device code page and ignores other kinds", () => {
    const device = browserRequest(
      "https://device.example.com/?user_code=ABCD",
      { host: "device.example.com", redirect: null, userCode: "ABCD" },
      "request-device",
    );
    expect(browserOpenDetails(device)).toEqual({
      host: "device.example.com",
      url: "https://device.example.com/?user_code=ABCD",
      userCode: "ABCD",
    });
    expect(
      browserOpenDetails(
        approval({
          id: "other",
          request: { kind: "monitor_start", monitor: "m", command: "tail" },
        }),
      ),
    ).toBeUndefined();
  });
});
