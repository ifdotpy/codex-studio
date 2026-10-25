// @ts-check
import { z } from "zod";
import { nativeUuid } from "./native-history.mjs";

// Supported CLI transcript structures for absence proofs. The SDK exposes
// SessionStoreEntry as opaque JSON and silently skips unsupported rows. Accept
// extension fields, but never an unknown discriminant or malformed payload.
const uuid = z.string().refine(nativeUuid);
const timestamp = z.iso.datetime({ offset: true });
const count = z.number().int().nonnegative();
const finite = z.number().nonnegative();
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
const tool = z.object({
  type: z.enum(["tool_use", "server_tool_use"]),
  id: z.string().min(1),
  name: z.string().min(1),
  input: z.record(z.string(), z.unknown()),
});
const block = z.union([
  text,
  image,
  document,
  tool,
  z.object({
    type: z.literal("thinking"),
    thinking: z.string(),
    signature: z.string(),
  }),
  z.object({ type: z.literal("redacted_thinking"), data: z.string() }),
  z.object({
    type: z.literal("tool_result"),
    tool_use_id: z.string().min(1),
    is_error: z.boolean().optional(),
    content: z
      .union([z.string(), z.array(z.union([text, image, document]))])
      .optional(),
  }),
]);
const content = z.union([z.string(), z.array(block)]);
const message = z.object({ role: z.enum(["user", "assistant"]), content });
const optionalFlags = {
  isMeta: z.boolean().optional(),
  isSidechain: z.boolean().optional(),
  isCompactSummary: z.boolean().optional(),
  isVisibleInTranscriptOnly: z.boolean().optional(),
  timestamp: timestamp.optional(),
};
const attachment = z.discriminatedUnion("type", [
  z.object({
    type: z.literal("queued_command"),
    source_uuid: uuid,
    prompt: content.optional(),
    isMeta: z.boolean().optional(),
  }),
  z.object({
    type: z.literal("environment"),
    snapshot: z.object({
      workingDirectory: z.string(),
      isWorktree: z.boolean(),
      isGitRepo: z.boolean(),
      additionalWorkingDirectories: z.array(z.string()),
      platform: z.string(),
      shell: z.string(),
      osVersion: z.string(),
    }),
  }),
  z.object({
    type: z.literal("model"),
    identity: z.object({
      modelId: z.string(),
      marketingName: z.string(),
      knowledgeCutoff: z.string(),
    }),
    text: z.string(),
  }),
  z.object({ type: z.literal("total_tokens_reminder"), text: z.string() }),
  z.object({
    type: z.literal("session_context"),
    context: z.record(z.string(), z.unknown()),
  }),
  z.object({ type: z.literal("date"), date: z.iso.date() }),
  z.object({
    type: z.literal("prompt_snapshot"),
    systemPrompt: z.array(z.string()),
    tools: z.array(z.record(z.string(), z.unknown())).optional(),
    reminderFold: z.boolean(),
    echoWireToolInputs: z.boolean(),
    contextRendering: z.string(),
  }),
]);
const modelUsage = z.object({
  inputTokens: count,
  outputTokens: count,
  thinkingTokens: count.optional(),
  cacheReadInputTokens: count,
  cacheCreationInputTokens: count,
  webSearchRequests: count,
  costUSD: finite,
});
const hex = z.string().regex(/^[a-f0-9]{64}$/);
const chain = {
  uuid,
  parentUuid: uuid.nullable(),
  sessionId: uuid,
  cwd: z.string().min(1),
  ...optionalFlags,
};
/** @type {Array<[string, z.ZodType]>} */
const structures = [
  [
    "user",
    z.object({
      ...chain,
      type: z.literal("user"),
      message: message.extend({ role: z.literal("user") }),
    }),
  ],
  [
    "assistant",
    z.object({
      ...chain,
      type: z.literal("assistant"),
      message: message.extend({
        role: z.literal("assistant"),
        id: z.string().optional(),
      }),
    }),
  ],
  [
    "attachment",
    z.object({ ...chain, type: z.literal("attachment"), attachment }),
  ],
  // Other system subtypes can carry input semantics. They remain unknown until
  // a schema and an installed-CLI witness establish their coverage.
  [
    "system",
    z.discriminatedUnion("subtype", [
      z.object({
        ...chain,
        type: z.literal("system"),
        subtype: z.literal("turn_duration"),
        durationMs: count,
      }),
      z.object({
        ...chain,
        type: z.literal("system"),
        subtype: z.literal("local_command"),
        content: z.string(),
        level: z.enum(["info", "warning", "error"]),
        commandRun: z.object({ command: z.string().min(1), args: z.string() }),
      }),
    ]),
  ],
  [
    "queue-operation",
    z.object({
      type: z.literal("queue-operation"),
      operation: z.enum(["enqueue", "dequeue", "remove", "clear"]),
      timestamp,
      sessionId: uuid,
    }),
  ],
  [
    "last-prompt",
    z.object({
      type: z.literal("last-prompt"),
      sessionId: uuid,
      lastPrompt: z.string(),
      leafUuid: uuid,
    }),
  ],
  [
    "atis-latch",
    z.object({
      type: z.literal("atis-latch"),
      sessionId: uuid,
      atis: z.string(),
    }),
  ],
  [
    "cost-state",
    z.object({
      type: z.literal("cost-state"),
      sessionId: uuid,
      totalCostUSD: finite,
      totalAPIDuration: finite,
      totalAPIDurationWithoutRetries: finite,
      totalToolDuration: finite,
      totalLinesAdded: count,
      totalLinesRemoved: count,
      totalDuration: finite,
      startTime: finite,
      modelUsage: z.record(z.string(), modelUsage),
      hasUnknownModelCost: z.boolean(),
    }),
  ],
  [
    "api-request-shape",
    z.object({
      type: z.literal("api-request-shape"),
      sessionId: uuid,
      shapeHash: hex,
      timestamp,
      version: z.string(),
      shape: z.object({
        system: z.array(text),
        tools: z.array(z.record(z.string(), z.unknown())),
      }),
    }),
  ],
  [
    "api-request-blob",
    z.object({
      type: z.literal("api-request-blob"),
      sessionId: uuid,
      hash: hex,
      message,
    }),
  ],
  [
    "api-request",
    z.object({
      type: z.literal("api-request"),
      sessionId: uuid,
      id: uuid,
      timestamp,
      version: z.string(),
      querySource: z.string(),
      shapeHash: hex,
      params: z.object({ model: z.string(), max_tokens: count }),
      keep: count,
      tail: z.array(hex),
      cacheControl: z.array(
        z.object({
          path: z.array(z.union([z.string(), count])),
          value: z.object({
            type: z.literal("ephemeral"),
            ttl: z.enum(["5m", "1h"]).optional(),
          }),
        }),
      ),
      requestDigest: hex,
    }),
  ],
  [
    "file-history-snapshot",
    z.object({
      type: z.literal("file-history-snapshot"),
      messageId: uuid,
      snapshot: z.object({
        messageId: uuid,
        trackedFileBackups: z.record(
          z.string(),
          z.object({
            backupFileName: z.string().nullable(),
            version: count,
            backupTime: timestamp.optional(),
          }),
        ),
        timestamp: timestamp.optional(),
      }),
    }),
  ],
  [
    "progress",
    z.object({
      ...chain,
      type: z.literal("progress"),
      data: z.discriminatedUnion("type", [
        z.object({
          type: z.literal("bash_progress"),
          output: z.string(),
          fullOutput: z.string(),
          elapsedTimeSeconds: finite,
          totalLines: count,
        }),
        z.object({
          type: z.literal("hook_progress"),
          hookName: z.string(),
          hookEvent: z.string(),
          command: z.string(),
        }),
      ]),
    }),
  ],
  [
    "custom-title",
    z.object({
      type: z.literal("custom-title"),
      sessionId: uuid,
      customTitle: z.string(),
    }),
  ],
  [
    "agent-name",
    z.object({
      type: z.literal("agent-name"),
      sessionId: uuid,
      agentName: z.string(),
    }),
  ],
  [
    "agent-color",
    z.object({
      type: z.literal("agent-color"),
      sessionId: uuid,
      agentColor: z.string(),
    }),
  ],
  [
    "tag",
    z.object({ type: z.literal("tag"), sessionId: uuid, tag: z.string() }),
  ],
  [
    "mode",
    z.object({ type: z.literal("mode"), sessionId: uuid, mode: z.string() }),
  ],
  [
    "pr-link",
    z.object({
      type: z.literal("pr-link"),
      sessionId: uuid,
      prUrl: z.url(),
      prNumber: count,
    }),
  ],
];
const schemas = new Map(structures);
/** @param {import("@anthropic-ai/claude-agent-sdk").SessionStoreEntry} row */
export function supportedHistoryRecord(row) {
  return schemas.get(row.type)?.safeParse(row).success === true;
}
