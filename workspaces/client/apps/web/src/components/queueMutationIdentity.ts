import type { paths } from "../generated/api";

type QueueMutation =
  paths["/api/queue"]["post"]["requestBody"]["content"]["application/json"];

export function queueMutationMessageId(
  mutation: QueueMutation,
): string | undefined {
  if (mutation.action === "reorder") return undefined;
  return mutation.message_id || mutation.id || undefined;
}
