import type { Approval } from "../../approvals";
import { approvalParams } from "../../approvals";
import type { JsonValue } from "../../types";
import type { AnswerQuestion } from "./AnswerFields";

type JsonObject = Record<string, JsonValue>;

function isJsonObject(value: unknown): value is JsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function objectValue(value: unknown): JsonObject {
  return isJsonObject(value) ? value : {};
}

function stringValue(value: unknown): string | undefined {
  return typeof value === "string" ? value : undefined;
}

/** An elicitation asks for a link when the provider says so; otherwise it is a form. */
export const isUrlElicitation = (approval: Approval) =>
  approval.request.kind === "elicitation" &&
  approvalParams(approval).mode === "url";

/** Whether the person answers with fields (questions or a form). */
export function isFieldRequest(approval: Approval): boolean {
  const { kind } = approval.request;
  return (
    kind === "question" ||
    kind === "agent_question" ||
    (kind === "elicitation" && !isUrlElicitation(approval))
  );
}

export type ElicitationSchema = {
  required: string[];
  properties: Record<string, { type?: unknown; items?: { enum?: unknown[] } }>;
};

/** The JSON schema of a form elicitation, read to the parts the form uses. */
export function elicitationSchema(
  approval: Approval,
): ElicitationSchema | undefined {
  if (approval.request.kind !== "elicitation") return undefined;
  const schema = objectValue(approval.request.schema);
  const properties = objectValue(schema.properties);
  return {
    required: Array.isArray(schema.required)
      ? schema.required.filter((id): id is string => typeof id === "string")
      : [],
    properties: Object.fromEntries(
      Object.entries(properties).map(([id, entry]) => {
        const value = objectValue(entry);
        const items = objectValue(value.items);
        return [
          id,
          {
            type: value.type,
            ...(Array.isArray(items.enum)
              ? { items: { enum: items.enum } }
              : {}),
          },
        ];
      }),
    ),
  };
}

/** The fields of a form elicitation: one per property of its JSON schema. */
function elicitationFields(schema: unknown): AnswerQuestion[] {
  const properties = objectValue(objectValue(schema).properties);
  return Object.entries(properties).map(([id, entry]) => {
    const value = objectValue(entry);
    const items = objectValue(value.items);
    const optionValues = Array.isArray(
      value.type === "array" ? items.enum : value.enum,
    )
      ? ((value.type === "array" ? items.enum : value.enum) as JsonValue[])
      : [];
    return {
      id,
      question: stringValue(value.title) || id,
      options: optionValues
        .filter((label): label is string => typeof label === "string")
        .map((label) => ({ label })),
      multiSelect: value.type === "array" && Array.isArray(items.enum),
      isSecret:
        value.isSecret === true ||
        value.writeOnly === true ||
        value.format === "password",
    };
  });
}

/** The fields the answer form shows, by request kind. */
export function requestQuestions(approval: Approval): AnswerQuestion[] {
  const { request } = approval;
  switch (request.kind) {
    case "question":
    case "agent_question":
      return request.questions.map((entry) => ({
        id: entry.id,
        question: entry.question || entry.id,
        options: entry.options.map((option) => ({
          label: option.label,
          ...(option.description ? { description: option.description } : {}),
        })),
        multiSelect: entry.multi_select === true,
        isSecret: entry.is_secret,
        isOther: request.kind === "agent_question" || entry.is_other === true,
      }));
    case "elicitation":
      return isUrlElicitation(approval)
        ? []
        : elicitationFields(request.schema);
    default:
      return [];
  }
}

/** What an approval card shows besides the question. Strings come from typed fields. */
export function requestApprovalDetails(approval: Approval) {
  const { request } = approval;
  const params = approvalParams(approval);
  return {
    command:
      request.kind === "exec" || request.kind === "monitor_start"
        ? request.command
        : undefined,
    cwd: request.kind === "exec" ? request.cwd : undefined,
    reason:
      request.kind === "exec" || request.kind === "patch"
        ? (request.reason ?? undefined)
        : undefined,
    message: stringValue(params.message),
    files: request.kind === "patch" ? request.files : undefined,
    permissions:
      request.kind === "permissions"
        ? (params.permissions ?? request.requested)
        : undefined,
    url:
      request.kind === "browser_open"
        ? request.url
        : isUrlElicitation(approval)
          ? stringValue(params.url)
          : undefined,
    tool: request.kind === "tool_use" ? request.tool : undefined,
  };
}

export type BrowserOpenDetails = {
  host: string;
  url: string;
  port?: number;
  /** The path of the loopback redirect that receives the sign-in result. */
  redirectPath?: string;
  clientId?: string;
  scope?: string;
  linuxUser?: string;
  /** "vm": a VM chat; "host": a native chat of the chat's server. */
  source?: string;
  /** The `user_code` of a device code page, so the person can type it. */
  userCode?: string;
  /** "waiting": the person opened the link in paste mode (docs/vm-browser-bridge.md, 6.3). */
  phase?: "waiting";
  /** The request deadline, in epoch seconds. */
  expiresAt?: number;
  /** The deadline of the waiting phase, in epoch seconds. */
  waitingExpiresAt?: number;
  /** A pasted result is on its way to the command. */
  pasteSent?: boolean;
  /** Agent IDs of other chats that wait on this card (one card for one sign-in). */
  joined?: string[];
};

const numberValue = (value: unknown): number | undefined =>
  typeof value === "number" && Number.isFinite(value) ? value : undefined;

/** The sign-in link of an agent (docs/vm-browser-bridge.md), or undefined. */
export function browserOpenDetails(
  approval: Approval,
): BrowserOpenDetails | undefined {
  if (approval.request.kind !== "browser_open") return undefined;
  const params = objectValue(approvalParams(approval));
  const host = stringValue(params.host);
  const url = approval.request.url;
  if (!host || !url) return undefined;
  const redirect = objectValue(params.redirect);
  const port = numberValue(redirect.port);
  const redirectPath = stringValue(redirect.path);
  const clientId = stringValue(params.clientId);
  const scope = stringValue(params.scope);
  const linuxUser = stringValue(params.linuxUser);
  const source = stringValue(params.source);
  const userCode = stringValue(params.userCode);
  const expiresAt = numberValue(params.expiresAt);
  const waitingExpiresAt = numberValue(objectValue(params.waiting).expiresAt);
  const joined = Array.isArray(params.joined)
    ? params.joined.filter(
        (agent): agent is string => typeof agent === "string",
      )
    : [];
  return {
    host,
    url,
    ...(port !== undefined ? { port } : {}),
    ...(port !== undefined && redirectPath ? { redirectPath } : {}),
    ...(clientId ? { clientId } : {}),
    ...(scope ? { scope } : {}),
    ...(linuxUser ? { linuxUser } : {}),
    ...(source ? { source } : {}),
    ...(userCode ? { userCode } : {}),
    ...(params.phase === "waiting" ? { phase: "waiting" as const } : {}),
    ...(expiresAt !== undefined ? { expiresAt } : {}),
    ...(waitingExpiresAt !== undefined ? { waitingExpiresAt } : {}),
    ...(isJsonObject(params.paste) ? { pasteSent: true } : {}),
    ...(joined.length ? { joined } : {}),
  };
}
