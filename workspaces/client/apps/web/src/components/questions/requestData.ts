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

function stringValues(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((entry): entry is string => typeof entry === "string")
    : [];
}

function jsonOptions(value: unknown): AnswerQuestion["options"] {
  if (!Array.isArray(value)) return undefined;
  return value.flatMap((entry) => {
    if (typeof entry === "string") return [{ label: entry }];
    if (!isJsonObject(entry)) return [];
    const label = stringValue(entry.label);
    if (!label) return [];
    const description = stringValue(entry.description);
    return [{ label, ...(description ? { description } : {}) }];
  });
}

export function requestQuestions(request: RequestDto): AnswerQuestion[] {
  const params = objectValue(request.params);
  if (Array.isArray(params.questions)) {
    return params.questions.flatMap((entry, index) => {
      if (!isJsonObject(entry)) return [];
      const id = stringValue(entry.id) || `question-${index + 1}`;
      return [
        {
          id,
          question:
            stringValue(entry.question) || stringValue(entry.title) || id,
          options: jsonOptions(entry.options),
          multiSelect: entry.multiSelect === true,
          isSecret: entry.isSecret === true,
          isOther:
            request.method === "agent/asyncQuestion" || entry.isOther === true,
        },
      ];
    });
  }

  const schema = objectValue(params.requestedSchema);
  const properties = objectValue(schema.properties);
  return Object.entries(properties).flatMap(([id, value]) => {
    if (!isJsonObject(value)) return [];
    const items = objectValue(value.items);
    const optionValues = stringValues(
      value.type === "array" ? items.enum : value.enum,
    );
    return [
      {
        id,
        question: stringValue(value.title) || id,
        options: optionValues.map((label) => ({ label })),
        multiSelect: value.type === "array" && Array.isArray(items.enum),
        isSecret:
          value.isSecret === true ||
          value.writeOnly === true ||
          value.format === "password",
      },
    ];
  });
}

export function requestApprovalDetails(request: RequestDto) {
  const params = objectValue(request.params);
  const preview = objectValue(request.preview);
  return {
    command: params.command ?? preview.command,
    permissions: params.permissions ?? preview.changes,
  };
}

export const BROWSER_OPEN_METHOD = "browser/open";

export type BrowserOpenDetails = {
  host: string;
  url: string;
  port?: number;
  clientId?: string;
  scope?: string;
  linuxUser?: string;
  /** "vm": a VM chat; "host": a native chat of the chat's server. */
  source?: string;
};

/** The sign-in link of a VM agent (docs/vm-browser-bridge.md), or undefined. */
export function browserOpenDetails(
  request: RequestDto,
): BrowserOpenDetails | undefined {
  if (request.method !== BROWSER_OPEN_METHOD) return undefined;
  const params = objectValue(request.params);
  const host = stringValue(params.host);
  const url = stringValue(params.url);
  if (!host || !url) return undefined;
  const redirect = objectValue(params.redirect);
  const port = typeof redirect.port === "number" ? redirect.port : undefined;
  const clientId = stringValue(params.clientId);
  const scope = stringValue(params.scope);
  const linuxUser = stringValue(params.linuxUser);
  const source = stringValue(params.source);
  return {
    host,
    url,
    ...(port !== undefined ? { port } : {}),
    ...(clientId ? { clientId } : {}),
    ...(scope ? { scope } : {}),
    ...(linuxUser ? { linuxUser } : {}),
    ...(source ? { source } : {}),
  };
}
