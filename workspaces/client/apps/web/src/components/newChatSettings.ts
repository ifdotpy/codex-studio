import { get, ApiError, type GetResult, type PostBody } from "../api";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";
import { serverCredentialAdapter } from "../servers/transport";
import type { StudioServer } from "../servers/registry";

export type NewChatSettings = Pick<
  PostBody<"/api/conversation">,
  | "model"
  | "effort"
  | "fast_mode"
  | "daybreak_enabled"
  | "worker_defaults"
  | "review_defaults"
> & { account_key?: string; workspaceMode: "layr" | "image" | "worktree" };

export function defaultWorkspaceMode(system?: string) {
  return system === "Darwin" ? "layr" : "worktree";
}

type ChatReadOptions = {
  query?: { account_key?: string; workers?: string; retry?: "1" };
  signal?: AbortSignal;
};
type ChatReadPath =
  | "/api/accounts"
  | "/api/models"
  | "/api/projects"
  | "/api/ui-summary";
export async function readNewChatResource<P extends ChatReadPath>(
  server: StudioServer | undefined,
  path: P,
  options: ChatReadOptions = {},
): Promise<GetResult<P>> {
  if (!server || server.id === "local") {
    const value =
      path === "/api/models"
        ? await get("/api/models", options)
        : path === "/api/accounts"
          ? await get("/api/accounts", { signal: options.signal })
          : path === "/api/projects"
            ? await get("/api/projects", { signal: options.signal })
            : await get("/api/ui-summary", { signal: options.signal });
    return value as GetResult<P>;
  }
  const url = new URL(path, server.origin);
  const query = "query" in options ? options.query : undefined;
  if (query)
    for (const [key, value] of Object.entries(query)) {
      if (value !== undefined && value !== null)
        url.searchParams.set(key, String(value));
    }
  const response = await serverCredentialAdapter().fetch(
    server,
    new Request(url, {
      headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH },
      signal: options.signal
        ? AbortSignal.any([options.signal, AbortSignal.timeout(15000)])
        : AbortSignal.timeout(15000),
      redirect: "error",
    }),
  );
  const value = await response.json();
  if (response.headers.get(API_SCHEMA_HASH_HEADER) !== API_SCHEMA_HASH)
    throw new Error("The selected server requires a Studio update.");
  if (!response.ok) throw new ApiError(value, response.status);
  return value as GetResult<P>;
}

const record = (value: unknown): value is Record<string, unknown> =>
  !!value && typeof value === "object" && !Array.isArray(value);
const nullableName = (value: unknown) =>
  value === null || (typeof value === "string" && !!value.trim());
export function isNewChatSettings(value: unknown): value is NewChatSettings {
  if (!record(value)) return false;
  const allowed = [
    "workspaceMode",
    "account_key",
    "model",
    "effort",
    "fast_mode",
    "daybreak_enabled",
    "worker_defaults",
    "review_defaults",
  ];
  if (Object.keys(value).some((key) => !allowed.includes(key))) return false;
  if (!["layr", "image", "worktree"].includes(String(value.workspaceMode)))
    return false;
  if (
    value.account_key !== undefined &&
    (typeof value.account_key !== "string" || !value.account_key.trim())
  )
    return false;
  if (
    ["model", "effort"].some(
      (key) => value[key] !== undefined && !nullableName(value[key]),
    )
  )
    return false;
  if (
    ["fast_mode", "daybreak_enabled"].some(
      (key) => value[key] !== undefined && typeof value[key] !== "boolean",
    )
  )
    return false;
  const worker = value.worker_defaults;
  if (
    worker !== undefined &&
    (!record(worker) ||
      Object.keys(worker).some(
        (key) =>
          ![
            "model",
            "effort",
            "fast_mode",
            "daybreak_enabled",
            "account_key",
          ].includes(key),
      ) ||
      !nullableName(worker.model) ||
      !nullableName(worker.effort) ||
      typeof worker.fast_mode !== "boolean" ||
      (worker.daybreak_enabled !== undefined &&
        typeof worker.daybreak_enabled !== "boolean") ||
      (worker.account_key !== undefined && !nullableName(worker.account_key)))
  )
    return false;
  const review = value.review_defaults;
  return (
    review === undefined ||
    (record(review) &&
      Object.keys(review).length === 2 &&
      nullableName(review.model) &&
      nullableName(review.effort))
  );
}
