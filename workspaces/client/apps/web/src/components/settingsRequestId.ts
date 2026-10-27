import { randomUUID } from "../uuid";

type Intent = { value: string | boolean; request_id: string };

export function settingsRequestId(
  api: Pick<typeof import("../api"), "saved" | "save">,
  key: string,
  value: Intent["value"],
): string {
  // Retry only the latest unresolved intent; a new selection supersedes it.
  const intent = api.saved<Intent | null>(key, null);
  const requestId = intent?.value === value ? intent.request_id : randomUUID();
  api.save(key, { value, request_id: requestId });
  return requestId;
}

export function clearSettingsRequest(
  api: Pick<typeof import("../api"), "saved" | "save">,
  key: string,
  requestId: string,
): void {
  // A late response must not erase a newer unresolved intent.
  if (api.saved<Intent | null>(key, null)?.request_id === requestId)
    api.save(key, null);
}
