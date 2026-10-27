import type { components } from "../../generated/api";
import type { JsonValue } from "../../types";
import type { AnswerQuestion } from "./AnswerFields";

type RequestDto = components["schemas"]["RequestEntityDto"];
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

export function requestQuestions(request: RequestDto): AnswerQuestion[] {
  const params = request.params;
  if (params?.questions) {
    return params.questions.map((entry, index) => {
      const id = entry.id || `question-${index + 1}`;
      return {
        id,
        question: entry.question || stringValue(entry.title) || id,
        options: entry.options?.flatMap((option) => {
          if (!option.label) return [];
          return [
            {
              label: option.label,
              ...(option.description
                ? { description: option.description }
                : {}),
            },
          ];
        }),
        multiSelect: entry.multiSelect === true,
        isSecret: entry.isSecret === true,
        isOther:
          request.method === "agent/asyncQuestion" || entry.isOther === true,
      };
    });
  }
  return Object.entries(params?.requestedSchema?.properties ?? {}).map(
    ([id, value]) => {
      const optionValues =
        (value.type === "array" ? value.items?.enum : value.enum) ?? [];
      return {
        id,
        question: value.title || id,
        options: optionValues
          .filter((label): label is string => typeof label === "string")
          .map((label) => ({ label })),
        multiSelect: value.type === "array" && value.items?.enum != null,
        isSecret:
          value.isSecret === true ||
          value.writeOnly === true ||
          value.format === "password",
      };
    },
  );
}

export function requestApprovalDetails(request: RequestDto) {
  return {
    command: request.params?.command ?? request.preview?.command,
    permissions: request.params?.permissions ?? request.preview?.changes,
  };
}

export const BROWSER_OPEN_METHOD = "browser/open";

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
  request: RequestDto,
): BrowserOpenDetails | undefined {
  if (request.method !== BROWSER_OPEN_METHOD) return undefined;
  const params = objectValue(request.params);
  const host = stringValue(params.host);
  const url = stringValue(params.url);
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
