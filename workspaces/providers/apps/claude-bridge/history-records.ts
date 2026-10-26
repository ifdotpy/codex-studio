import type { SessionStoreEntry } from "@anthropic-ai/claude-agent-sdk";
import { z } from "zod";
import { nativeUuid } from "./native-history.ts";

// H2 strictness follows input provenance, not the CLI's evolving output formats.
// SDK SessionStoreEntry is opaque JSON. Input-bearing headers and known payload
// fields are checked strictly; non-input payloads are deliberately not parsed.
const uuid = z.string().refine(nativeUuid);
const timestamp = z.iso.datetime({ offset: true });
const text = z.object({ type: z.literal("text"), text: z.string() });
const image = z.object({
  type: z.literal("image"),
  source: z.discriminatedUnion("type", [
    z.object({
      type: z.literal("base64"),
      media_type: z.enum([
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
      ]),
      data: z.string(),
    }),
    z.object({ type: z.literal("url"), url: z.url() }),
  ]),
});
const document = z.object({
  type: z.literal("document"),
  source: z.discriminatedUnion("type", [
    z.object({
      type: z.literal("base64"),
      media_type: z.literal("application/pdf"),
      data: z.string(),
    }),
    z.object({
      type: z.literal("text"),
      media_type: z.literal("text/plain"),
      data: z.string(),
    }),
    z.object({ type: z.literal("url"), url: z.url() }),
  ]),
});
const promptBlock = z.union([
  text,
  image,
  document,
  z.object({
    type: z.literal("tool_result"),
    tool_use_id: z.string().min(1),
    is_error: z.boolean().optional(),
    content: z
      .union([z.string(), z.array(z.union([text, image, document]))])
      .optional(),
  }),
]);
const prompt = z.union([z.string(), z.array(promptBlock)]);
const origin = z.object({ kind: z.string().min(1) });
const flags = {
  isMeta: z.boolean().optional(),
  isSidechain: z.boolean().optional(),
  isCompactSummary: z.boolean().optional(),
  isVisibleInTranscriptOnly: z.boolean().optional(),
  timestamp: timestamp.optional(),
};
const chain = {
  type: z.string(),
  uuid,
  parentUuid: uuid.nullable(),
  sessionId: uuid,
  cwd: z.string().min(1),
  isSidechain: z.boolean(),
};
const basicChain = z.object(chain);
const basicMetadata = z.object({
  type: z.string(),
  uuid: uuid.optional(),
  parentUuid: uuid.nullable().optional(),
  sessionId: uuid.optional(),
  cwd: z.string().optional(),
  isSidechain: z.boolean().optional(),
});
const promptRecord = z.object({
  ...chain,
  ...flags,
  type: z.literal("user"),
  message: z.object({
    role: z.literal("user"),
    content: prompt,
    id: z.string().optional(),
    stop_reason: z.string().nullable().optional(),
  }),
  promptId: uuid.optional(),
  origin: origin.optional(),
  permissionMode: z.string().optional(),
  parent_tool_use_id: z.string().nullable().optional(),
  parent_agent_id: z.string().nullable().optional(),
});
// Claude 2.1.310 jPo records exactly these queue payload fields. In particular,
// SDK absorption accounting consumes reason, commandUuid and deliveryId.
const queueRecord = z.strictObject({
  type: z.literal("queue-operation"),
  operation: z.enum(["enqueue", "dequeue", "remove", "clear"]),
  timestamp,
  sessionId: uuid,
  content: prompt.optional(),
  commandUuid: uuid.optional(),
  reason: z.string().optional(),
  deliveryId: z.string().min(1).optional(),
});
const queuedCommand = z.object({
  ...chain,
  ...flags,
  type: z.literal("attachment"),
  attachment: z.strictObject({
    type: z.literal("queued_command"),
    source_uuid: uuid,
    prompt: prompt.optional(),
    isMeta: z.boolean().optional(),
    origin: origin.optional(),
    commandMode: z.string().optional(),
    runId: z.string().optional(),
    deliveryId: z.string().optional(),
  }),
});
// Non-input attachment kinds have a closed outer payload shape. Their named
// provider-owned objects remain opaque; new kinds or payload keys need review.
const opaqueObject = z.record(z.string(), z.unknown());
const names = z.array(z.string());
const nonInputAttachment = z.discriminatedUnion("type", [
  z.strictObject({
    type: z.literal("environment"),
    snapshot: z.object({
      workingDirectory: z.string(),
      isWorktree: z.boolean(),
      isGitRepo: z.boolean(),
      additionalWorkingDirectories: names,
      platform: z.string(),
      shell: z.string(),
      osVersion: z.string(),
    }),
  }),
  z.strictObject({
    type: z.literal("model"),
    identity: z.object({
      modelId: z.string(),
      marketingName: z.string(),
      knowledgeCutoff: z.string(),
    }),
    text: z.string(),
  }),
  z.strictObject({
    type: z.literal("total_tokens_reminder"),
    text: z.string(),
  }),
  z.strictObject({ type: z.literal("session_context"), context: opaqueObject }),
  z.strictObject({ type: z.literal("date"), date: z.iso.date() }),
  z.strictObject({
    type: z.literal("prompt_snapshot"),
    systemPrompt: names,
    tools: z.array(opaqueObject).optional(),
    cliPrefix: z.string().optional(),
    reminderFold: z.boolean(),
    echoWireToolInputs: z.boolean(),
    contextRendering: z.string(),
  }),
  z.strictObject({
    type: z.literal("deferred_tools_delta"),
    addedNames: names,
    addedLines: names,
    removedNames: names,
    wireHiddenNames: names.optional(),
    readdedNames: names.optional(),
    pendingMcpServers: names.optional(),
    needsAuthMcpServers: names.optional(),
    failedMcpServers: names.optional(),
  }),
  z.strictObject({
    type: z.literal("deferred_tools_record"),
    entries: z.array(opaqueObject),
  }),
]);
const metadataTypes = new Set([
  "summary",
  "file-history-snapshot",
  "last-prompt",
  "custom-title",
  "agent-name",
  "agent-color",
  "tag",
  "mode",
  "pr-link",
  "atis-latch",
  "cost-state",
  "api-request-shape",
  "api-request-blob",
  "api-request",
]);
export type HistoryRecordKind = "input" | "nonInput" | "metadata" | "queue";
export const object = (value: unknown): value is Record<string, unknown> =>
  !!value && typeof value === "object" && !Array.isArray(value);
/** Tool receipts cannot carry a Studio instruction, including ToolSearch's new
 * inner block types. Do not validate their provider-owned payloads as prompts.
 * @param {import("@anthropic-ai/claude-agent-sdk").SessionStoreEntry} row
 */
function onlyToolResults(row: SessionStoreEntry) {
  const body = row.message;
  return (
    object(body) &&
    Array.isArray(body.content) &&
    body.content.length > 0 &&
    body.content.every((block) => object(block) && block.type === "tool_result")
  );
}
/** @param {import("@anthropic-ai/claude-agent-sdk").SessionStoreEntry} row
 * @returns {HistoryRecordKind | null}
 */
export function historyRecordKind(
  row: SessionStoreEntry,
): HistoryRecordKind | null {
  if (row.type === "queue-operation")
    return queueRecord.safeParse(row).success ? "queue" : null;
  if (row.type === "user") {
    if (row.isCompactSummary === true || onlyToolResults(row))
      return basicChain.safeParse(row).success ? "nonInput" : null;
    return promptRecord.safeParse(row).success ? "input" : null;
  }
  if (row.type === "attachment") {
    const attachment = row.attachment;
    if (!object(attachment) || typeof attachment.type !== "string") return null;
    if (attachment.type === "queued_command")
      return queuedCommand.safeParse(row).success ? "input" : null;
    return basicChain.safeParse(row).success &&
      nonInputAttachment.safeParse(attachment).success
      ? "nonInput"
      : null;
  }
  if (["assistant", "system", "progress"].includes(row.type))
    return basicChain.safeParse(row).success ? "nonInput" : null;
  return metadataTypes.has(row.type) && basicMetadata.safeParse(row).success
    ? "metadata"
    : null;
}
