import type {
  AccountInfo,
  ElicitationRequest,
  ElicitationResult,
  McpServerStatus,
  ModelInfo,
  Options,
  SDKTaskStartedMessage,
  SDKTaskUpdatedMessage,
  SDKTaskNotificationMessage,
  SDKBackgroundTasksChangedMessage,
  SDKRateLimitInfo,
  SDKControlGetUsageResponse,
  Query,
  SlashCommand,
} from "@anthropic-ai/claude-agent-sdk";
import type {
  AskUserQuestionInput,
  TodoWriteInput,
} from "@anthropic-ai/claude-agent-sdk/sdk-tools";
import type {
  Item,
  Session,
  Thread,
  Turn,
  ControlReceipt,
  JournalRepairReport,
} from "./session-state.ts";
import type { UsageObservation } from "./usage-contract.generated.ts";
import type {
  CacheReport,
  ToolDefinition,
  RateLimitBucketDto,
  RateLimitWindowDto,
} from "./runtime-contract.generated.ts";
import type {
  HUMAN_INPUT,
  STUDIO_EVENT_INPUT,
} from "./input-origin.generated.ts";
import type { MoveProof } from "./move.ts";
import type { ExternalServer, CatalogDependencies } from "./move-external.ts";

// These types describe the bridge's wire values, not unvalidated JSON ingress.
// Command executor messages pass through unchanged and remain unknown here.
export type BridgeRpcId = string | number;
export type BridgeEmpty = Record<string, never>;
export interface BridgeError {
  code: number;
  message: string;
  data?: unknown;
}
export interface BridgeSuccessResponse<R> {
  id: BridgeRpcId;
  result: R;
}
export interface BridgeErrorResponse {
  id: BridgeRpcId;
  error: BridgeError;
}
export type BridgeResponse<R = unknown> =
  | BridgeSuccessResponse<R>
  | BridgeErrorResponse;
export type BridgeInputOrigin = typeof HUMAN_INPUT | typeof STUDIO_EVENT_INPUT;
export type BridgeInput = (
  | { type: "text"; text: string }
  | { type: "localImage"; path: string }
) & { inputOrigin?: BridgeInputOrigin | undefined };
export interface BridgePage<T> {
  data: T[];
  nextCursor: string | null;
}
export interface BridgePageParams {
  cursor?: string | null | undefined;
  limit?: number | undefined;
}
export interface BridgeThreadParams {
  threadId: string;
}
export type BridgeSessionParams = Partial<
  Pick<
    Session,
    | "model"
    | "approvalPolicy"
    | "activePermissionProfile"
    | "sandbox"
    | "claude"
    | "developerInstructions"
    | "ephemeral"
    | "capabilityProfile"
  >
> & { cwd?: string | undefined; dynamicTools?: ToolDefinition[] | undefined };
export interface BridgeTurnParams extends BridgeThreadParams {
  dynamicTools?: ToolDefinition[] | undefined;
  input: BridgeInput[];
  clientUserMessageId?: string | undefined;
  model?: Options["model"] | undefined;
  effort?: Options["effort"] | undefined;
  serviceTier?: string | undefined;
  claudeCommand?: string | undefined;
  idleCompaction?: Pick<CacheReport, "prefix" | "requestId"> | undefined;
}
export type BridgeThreadResult = {
  thread: Thread;
  model: Session["model"];
  sandbox: null;
  approvalPolicy: string | null | undefined;
  activePermissionProfile: Session["activePermissionProfile"];
  reattached?: true | undefined;
};
export type BridgeTurnResult = {
  turn: Pick<Turn, "id" | "status">;
  steered?: true | undefined;
};
// The bridge merges task events and their patches into one mutable task view.
export type BridgeTask =
  | Partial<
      Omit<SDKTaskStartedMessage, "subtype"> &
        SDKTaskUpdatedMessage["patch"] &
        Pick<SDKTaskNotificationMessage, "output_file">
    >
  | SDKBackgroundTasksChangedMessage["tasks"][number];
export type BridgeCommand = Required<
  Pick<
    SlashCommand,
    "name" | "description" | "argumentHint" | "aliases" | "builtin"
  >
>;
export type BridgeRateLimitInfo = Omit<
  Partial<SDKRateLimitInfo>,
  "rateLimitType" | "utilization"
> & {
  rateLimitType?: string | undefined;
  utilization?: number | null | undefined;
};
export type BridgeRateWindow = Pick<
  RateLimitWindowDto,
  "usedPercent" | "resetsAt" | "windowDurationMins"
>;
export type BridgeRateBucket = Pick<
  RateLimitBucketDto,
  "primary" | "secondary"
> & {
  [K in "limitId" | "limitName"]: NonNullable<RateLimitBucketDto[K]>;
} & { claudeRateLimit?: BridgeRateLimitInfo | undefined };
export interface BridgeRateLimits {
  rateLimits: BridgeRateBucket;
  rateLimitsByLimitId: Record<string, BridgeRateBucket>;
  subscriptionType?:
    | SDKControlGetUsageResponse["subscription_type"]
    | undefined;
  extraUsage?:
    | Partial<
        NonNullable<
          NonNullable<SDKControlGetUsageResponse["rate_limits"]>["extra_usage"]
        >
      >
    | null
    | undefined;
  accountId?: string | undefined;
  claudeRateLimit?: BridgeRateLimitInfo | undefined;
}
export type BridgeModel = {
  id: ModelInfo["value"];
  model: ModelInfo["value"];
  resolvedModel: string | null;
  displayName: string;
  description: ModelInfo["description"];
  isDefault: boolean;
  hidden: false;
  supportedReasoningEfforts: { reasoningEffort: string; description: string }[];
  defaultReasoningEffort: "medium" | null;
  serviceTiers: { id: "priority"; name: "Fast" }[];
};
export type BridgeMoveProof = MoveProof & {
  compiledToolsJson?: string | undefined;
  externalCatalog?: ExternalServer[] | undefined;
  externalTransports?: CatalogDependencies["transports"] | undefined;
  options: {
    tools: string[];
    studioCatalogHash?: string | undefined;
    externalCatalog?: ExternalServer[] | undefined;
    disallowedTools: string[];
    settingSources: NonNullable<Options["settingSources"]>;
    strictMcpConfig: true;
    model: Session["model"];
    permissionMode: Options["permissionMode"];
    settings: Parameters<Query["applyFlagSettings"]>[0];
    effort: Options["effort"] | null;
    systemPrompt: {
      type: "preset";
      preset: "claude_code";
      append: string;
      snapshot: true;
      excludeDynamicSections: true;
    };
    extraArgs: NonNullable<Options["extraArgs"]>;
  };
  optionsHash: string;
  proofMethod:
    | "saved_snapshot_and_current_ordered_mcp_catalogs"
    | "saved_snapshot_and_identical_studio_options";
};
export type BridgeMoveSession = Session & {
  moveProof: Omit<BridgeMoveProof, "session">;
  dynamicToolsJson: string;
};
export interface BridgeInterruptReceipt {
  cancelled: string[];
  still_queued: string[];
}
export type BridgeRepairReport = Omit<
  JournalRepairReport,
  "status" | "scopeConfirmed" | "nativeHistoryConfirmed"
> & {
  status: JournalRepairReport["status"] | "none";
  scopeConfirmed: boolean | null;
  nativeHistoryConfirmed: boolean | null;
};
export interface BridgeState {
  settings: NonNullable<Session["claude"]>;
  turns: (Pick<Turn, "id" | "status" | "error"> & {
    canRestoreFiles: boolean;
    restoreFilesUnavailableReason: string | null;
    text: string;
  })[];
  tasks: BridgeTask[];
  version?: string | undefined;
  nativeId: string;
  nativeSessionOutcome?: "unknown" | undefined;
  nativeSessionError?: string | undefined;
  contextWindow?: number | undefined;
  totalTokens?: number | undefined;
}

// Each pair is [accepted params, successful result]. Responses on stdout carry
// only id/result; the method discriminator is retained by the awaiting caller.
export interface BridgeMethods {
  initialize: [
    unknown,
    {
      userAgent: "studio-claude-bridge";
      platform: NodeJS.Platform;
      capabilities: {
        claudeVersion: 23;
        passiveAccountRead: true;
        activityRead: true;
      };
    },
  ];
  initialized: [unknown, BridgeEmpty];
  "model/list": [unknown, BridgePage<BridgeModel>];
  "claude/activity": [unknown, { busy: boolean }];
  "account/read": [
    { passive?: boolean | undefined },
    {
      account: {
        type: "claude";
        email: AccountInfo["email"];
        organizationUuid?: string | undefined;
        accountUuid?: string | undefined;
        _credentialIdentity: string;
        planType: AccountInfo["subscriptionType"];
      };
      requiresOpenaiAuth: false;
    },
  ];
  "account/rateLimits/read": [{ cwd?: string | undefined }, BridgeRateLimits];
  "claude/commands": [{ cwd?: string | undefined }, BridgeCommand[]];
  "claude/moveVersions": [unknown, { cli: string; sdk: string }];
  "claude/moveIdentity": [unknown, { email: string; organizationId: string }];
  "claude/moveProof": [BridgeThreadParams, BridgeMoveProof];
  "claude/movePreflight": [
    { proof: BridgeMoveProof; model: string; cwd: string },
    Pick<BridgeMoveProof, "snapshotHash" | "optionsHash" | "proofMethod">,
  ];
  "claude/moveExport": [
    BridgeThreadParams,
    { path: string; session: BridgeMoveSession },
  ];
  "claude/moveImport": [
    { session: BridgeMoveSession; path: string; cwd: string },
    { thread: Thread },
  ];
  "claude/repairReport": [BridgeThreadParams, BridgeRepairReport];
  "claude/fileBackups": [
    BridgeThreadParams & { settledRequestIds?: string[] | null | undefined },
    {
      count: number;
      bytes: number;
      maxCount: number;
      maxBytes: number;
      released: number;
    },
  ];
  "claude/state": [BridgeThreadParams, BridgeState];
  "claude/diagnostics": [
    unknown,
    {
      liveQueries: number;
      activeTurns: number;
      backgroundQueries: number;
      idleQueries: number;
      sessionCache: {
        cachedSessions: number;
        retainedBytes: number;
        pendingWrites: number;
        unsavedSessions: number;
        journalBytes: number;
      };
    },
  ];
  "claude/settings": [
    BridgeThreadParams & { settings: NonNullable<Session["claude"]> },
    { settings: NonNullable<Session["claude"]> },
  ];
  "thread/rollback": [
    BridgeThreadParams & {
      requestId: string;
      turnId: string;
      restoreFiles?: boolean | undefined;
      fileMessageId?: string | undefined;
    },
    NonNullable<ControlReceipt["result"]>,
  ];
  "thread/start": [
    BridgeSessionParams & { cwd: string },
    Omit<BridgeSessionParams, "sandbox" | "model"> & {
      thread: Thread;
      model: Session["model"];
      sandbox: null;
    },
  ];
  "thread/turns/list": [
    BridgeThreadParams &
      BridgePageParams & { sortDirection?: "asc" | "desc" | undefined },
    BridgePage<Turn>,
  ];
  "thread/turns/items/list": [
    BridgeThreadParams & BridgePageParams & { turnId: string },
    BridgePage<Item>,
  ];
  "thread/list": [BridgePageParams, BridgePage<Thread>];
  "skills/extraRoots/set": [unknown, never];
  "thread/resume": [
    BridgeThreadParams &
      BridgeSessionParams & { excludeTurns?: boolean | undefined },
    BridgeThreadResult,
  ];
  "thread/read": [
    BridgeThreadParams & { includeTurns?: boolean | undefined },
    BridgeThreadResult,
  ];
  "thread/unsubscribe": [
    BridgeThreadParams,
    { status: "unsubscribed" | "notLoaded" },
  ];
  "thread/loaded/list": [unknown, BridgePage<string>];
  "thread/fork": [
    BridgeThreadParams &
      BridgeSessionParams & {
        lastTurnId?: string | null | undefined;
        excludeTurns?: boolean | undefined;
      },
    BridgeThreadResult,
  ];
  "thread/compact/start": [
    BridgeThreadParams & {
      requestId?: string | undefined;
      idleCompaction?: BridgeTurnParams["idleCompaction"];
    },
    BridgeTurnResult,
  ];
  "turn/start": [BridgeTurnParams, BridgeTurnResult];
  "turn/steer": [
    BridgeThreadParams & {
      expectedTurnId: string;
      clientUserMessageId: string;
      input: BridgeInput[];
    },
    { turnId: string },
  ];
  "turn/interrupt": [
    BridgeThreadParams & {
      turnId?: string | undefined;
      stopBackgroundTasks?: boolean | undefined;
    },
    BridgeInterruptReceipt,
  ];
  "thread/name/set": [BridgeThreadParams & { name: string }, BridgeEmpty];
  "thread/backgroundTerminals/list": [
    BridgeThreadParams,
    BridgePage<BridgeTask> & { terminals: BridgeTask[] },
  ];
  "thread/queue/list": [
    BridgeThreadParams,
    BridgePage<{ id: string; status: "queued" }> & {
      items: { id: string; status: "queued" }[];
    },
  ];
  "thread/commandExecution/background": [
    BridgeThreadParams & { turnId: string; itemId: string },
    { status: "backgrounded" | "not_running" },
  ];
  "claude/stopTask": [BridgeThreadParams & { taskId: string }, BridgeEmpty];
  "mcpServerStatus/list": [
    BridgeThreadParams & BridgePageParams,
    BridgePage<Omit<McpServerStatus, "config">>,
  ];
  "mcpServer/reconnect": [BridgeThreadParams & { name: string }, BridgeEmpty];
  "skills/list": [
    { cwds?: string[] | undefined },
    {
      data: {
        cwd: string;
        skills: {
          name: string;
          description: string;
          path: string;
          enabled: boolean;
        }[];
        errors: { message: string }[];
      }[];
    },
  ];
  "config/read": [Record<string, unknown>, unknown];
  "command/exec": [Record<string, unknown>, unknown];
  "command/exec/write": [Record<string, unknown>, unknown];
  "command/exec/resize": [Record<string, unknown>, unknown];
  "command/exec/terminate": [Record<string, unknown>, unknown];
}
export type BridgeMethod = keyof BridgeMethods;
export type BridgeParams<M extends BridgeMethod> = BridgeMethods[M][0];
export type BridgeResult<M extends BridgeMethod> = BridgeMethods[M][1];
export type BridgeRequest<M extends BridgeMethod = BridgeMethod> = {
  [K in M]: {
    id?: BridgeRpcId | undefined;
    method: K;
    params: BridgeParams<K>;
  };
}[M];
export type BridgeMethodResult<M extends BridgeMethod = BridgeMethod> = {
  [K in M]: { method: K; result: BridgeResult<K> };
}[M];

export interface BridgeItemEvent extends BridgeThreadParams {
  turnId?: string | undefined;
  item: Item;
  startedAtMs?: number | undefined;
  completedAtMs?: number | undefined;
}
export interface BridgeNotifications {
  "item/started": BridgeItemEvent;
  "item/completed": BridgeItemEvent;
  "turn/started": BridgeThreadParams & { turn: Pick<Turn, "id" | "status"> };
  "turn/completed": BridgeThreadParams & {
    turn: Pick<
      Turn,
      "id" | "status" | "error" | "startOutcome" | "clientUserMessageId"
    > & { unappliedSteers?: string[] | undefined };
  };
  "thread/tokenUsage/updated": BridgeThreadParams & {
    turnId?: string | null | undefined;
    studioUsage: UsageObservation;
  };
  "account/rateLimits/updated": BridgeRateLimits;
  "provider/generationStarted": BridgeThreadParams & {
    turnId: string;
    responseId: string;
  };
  "item/agentMessage/delta": BridgeThreadParams & {
    turnId: string;
    itemId: string;
    delta: string;
  };
  "thread/idleCompaction/updated": BridgeThreadParams & {
    turnId: string;
    report: CacheReport;
  };
  "turn/plan/updated": BridgeThreadParams & {
    turnId: string;
    plan: {
      step: TodoWriteInput["todos"][number]["content"];
      status: TodoWriteInput["todos"][number]["status"];
    }[];
  };
  "serverRequest/resolved": BridgeThreadParams & {
    requestId?: BridgeRpcId | undefined;
  };
  "command/exec/outputDelta": unknown;
}
export type BridgeNotificationMethod = keyof BridgeNotifications;
export type BridgeNotificationParams<M extends BridgeNotificationMethod> =
  BridgeNotifications[M];
export type BridgeNotification<
  M extends BridgeNotificationMethod = BridgeNotificationMethod,
> = {
  [K in M]: { method: K; params: BridgeNotificationParams<K> };
}[M];

export interface BridgeServerRequests {
  "item/tool/requestUserInput": [
    BridgeThreadParams & {
      turnId: string;
      itemId: string;
      questions: (AskUserQuestionInput["questions"][number] & {
        id: string;
        isOther: true;
        isSecret: false;
      })[];
    },
    { answers?: Record<string, { answers: string[] }> | undefined },
  ];
  "item/commandExecution/requestApproval": [
    BridgeThreadParams & {
      turnId: string;
      itemId: string;
      command: string;
      cwd: string;
      reason: string;
    },
    { decision: "accept" | "acceptForSession" | "decline" | "cancel" },
  ];
  "item/tool/call": [
    BridgeThreadParams & {
      turnId?: string | undefined;
      callId: string;
      tool: string;
      arguments: Record<string, unknown>;
    },
    {
      success?: boolean | undefined;
      contentItems?:
        | ({ text?: string | undefined } & Record<string, unknown>)[]
        | undefined;
    },
  ];
  "mcpServer/elicitation/request": [
    BridgeThreadParams & {
      turnId?: string | undefined;
      request: ElicitationRequest;
    } & ElicitationRequest,
    ElicitationResult,
  ];
}
export type BridgeServerMethod = keyof BridgeServerRequests;
export type BridgeServerParams<M extends BridgeServerMethod> =
  BridgeServerRequests[M][0];
export type BridgeServerResult<M extends BridgeServerMethod> =
  BridgeServerRequests[M][1];
export type BridgeServerRequest<
  M extends BridgeServerMethod = BridgeServerMethod,
> = {
  [K in M]: { id: BridgeRpcId; method: K; params: BridgeServerParams<K> };
}[M];
