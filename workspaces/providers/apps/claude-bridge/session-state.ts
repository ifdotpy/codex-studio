import type { StallReport } from "./stall-watch.ts";
import type {
  CacheReport,
  ToolDefinition,
} from "./runtime-contract.generated.ts";
import type {
  Options,
  RewindFilesResult,
  SessionMessage,
  SessionStartHookInput,
} from "@anthropic-ai/claude-agent-sdk";

// The bridge owns its mutable mirror and recovery receipts. Native messages,
// settings, and file-rewind results retain their SDK-owned contracts.
export type TransitionSource = "init" | SessionStartHookInput["source"];
export interface NativeInputAttempt {
  inputId: string;
  turnId: string;
  attemptId: string;
}
export interface PendingTransition extends Partial<NativeInputAttempt> {
  from: string;
  target?: string | undefined;
  source: TransitionSource | "input";
  status: "pending" | "unknown";
}
export interface FileIdentity {
  hash: string;
  mode: number;
  bytes: number;
}
export interface FileRestorePlan {
  cwd: string;
  directory: string;
  files: {
    file: string;
    before: FileIdentity | null;
    expected: FileIdentity | null;
    backup?: string | undefined;
  }[];
}
export interface TurnError {
  resetsAt?: number | undefined;
  message: string;
  codexErrorInfo?: string | undefined;
  data?:
    | (Record<string, unknown> & {
        claudeToolStall?: StallReport | undefined;
        claudeStopTimeout?: boolean | undefined;
      })
    | undefined;
}
export interface Item {
  id?: string | undefined;
  type?: string | undefined;
  nativeId?: string | undefined;
  nativeIds?: string[] | undefined;
  nativeSubmissionAttemptId?: string | undefined;
  delivery?: string | undefined;
  deliveryStatus?: string | undefined;
  content?: unknown | undefined;
  text?: string | undefined;
  thinking?: string | boolean | undefined;
  nativeSubagent?: boolean | undefined;
  canMoveToBackground?: boolean | undefined;
  backgroundTaskId?: string | undefined;
  backgroundedByUser?: boolean | undefined;
  outputFile?: string | undefined;
  stalled?: boolean | undefined;
  compactionMetadata?:
    | import("@anthropic-ai/claude-agent-sdk").SDKCompactBoundaryMessage["compact_metadata"]
    | undefined;
  clientId?: string | undefined;
  elapsedSeconds?: number | undefined;
  phase?: string | undefined;
  studioNotice?: boolean | undefined;
  error?: string | undefined;
  status?: string | undefined;
  tool?: string | undefined;
  arguments?: Record<string, unknown> | undefined;
  command?: string | undefined;
  cwd?: string | undefined;
  aggregatedOutput?: string | undefined;
  exitCode?: number | null | undefined;
  result?: unknown | undefined;
  server?: string | undefined;
  changes?: {
    path: string | undefined;
    kind: { type: string };
    diff: string;
  }[];
}
export interface Turn {
  synthetic?: boolean | undefined;
  id: string;
  clientUserMessageId?: string | undefined;
  status?: string | undefined;
  startOutcome?: string | undefined;
  items: Item[];
  nativeSessionId?: string | undefined;
  nativeHistoryDetached?: boolean | undefined;
  nativeMessageId?: string | undefined;
  fileCheckpointMessageId?: string | undefined;
  fileCheckpointSource?: { nativeId: string | undefined; cwd: string };
  idleCompaction?: boolean | undefined;
  interrupted?: boolean | undefined;
  error?: TurnError | undefined;
  stallError?: TurnError | undefined;
  limitError?:
    | { codexErrorInfo: string | undefined; resetsAt?: number }
    | undefined;
  apiErrorInfo?: string | undefined;
}
export interface ControlReceipt {
  kind: "rollback";
  turnId: string;
  restoreFiles?: boolean | undefined;
  fileMessageId?: string | undefined;
  status: "submitted" | "forked" | "rewind_submitted" | "rewound" | "completed";
  sourceNativeId: string;
  targetNativeId?: string | undefined;
  createdAt: string;
  retainedCount: number;
  sourceMessages?: SessionMessage[] | undefined;
  previousTurns?: Turn[] | undefined;
  upToMessageId: string | null;
  filePlan?: FileRestorePlan | undefined;
  fileResult?: RewindFilesResult | undefined;
  fileBackupReleaseAuthorized?: boolean | undefined;
  fileBackupReleased?: boolean | undefined;
  result?: {
    threadId: string;
    nativeId: string;
    removedTurns: number;
    filesRestored?: boolean | undefined;
    fileResult?: RewindFilesResult | undefined;
  };
}
export interface JournalRepairReport {
  status: "needs_repair" | "repaired";
  skippedRows: number;
  invalidSequence: boolean;
  scopeConfirmed: boolean;
  nativeHistoryConfirmed: boolean;
}
export interface Session {
  id: string;
  cwd: string;
  turns: Turn[];
  nativeId?: string | undefined;
  nativeHistoryCwd?: string | undefined;
  started?: boolean | undefined;
  ephemeral?: boolean | undefined;
  pendingNativeSession?: PendingTransition | undefined;
  nativeSessionTransitionError?: string | undefined;
  nativeSessionTransitions?: {
    from: string;
    to: string;
    source: TransitionSource;
  }[];
  controlRequests?: Record<string, ControlReceipt> | undefined;
  historyBranches?: {
    requestId: string | undefined;
    nativeId: string;
    turns: Turn[];
  }[];
  journalRepair?: JournalRepairReport | undefined;
  createdAt?: number | undefined;
  updatedAt?: number | undefined;
  preview?: string | undefined;
  name?: string | null | undefined;
  model?: Options["model"] | undefined;
  approvalPolicy?: string | undefined;
  activePermissionProfile?: unknown | undefined;
  sandbox?: string | undefined;
  claude?: import("./thinking.ts").ClaudeSettings | undefined;
  developerInstructions?: string | undefined;
  dynamicTools?: ToolDefinition[] | undefined;
  moveProof?: import("./bridge-protocol.ts").BridgeMoveProof | undefined;
  dynamicToolsJson?: string | undefined;
  capabilityProfile?: unknown | undefined;
  moveTurnOptions?: Partial<Options> | undefined;
  userMcp?: boolean | undefined;
  studioImageWorkspaceTempDir?: string | undefined;
  claudeVersion?: string | undefined;
  idleCache?: CacheReport | undefined;
  contextWindow?: number | undefined;
  totalTokens?: number | undefined;
}
export interface SessionMetadata {
  id: string;
  cwd: string;
  createdAt: Session["createdAt"];
  updatedAt: Session["updatedAt"];
  preview: string;
  name: string | null;
  model: Session["model"];
  approvalPolicy: string | null;
  activePermissionProfile: unknown;
  revision: string;
}
export interface Thread {
  id: string;
  cwd: string;
  createdAt: SessionMetadata["createdAt"];
  updatedAt: SessionMetadata["updatedAt"];
  preview: string;
  name: string | null;
  historyVersion: string;
  turns: Turn[];
  journalRepair?: JournalRepairReport | undefined;
  status: { type: "active" | "idle" };
  modelProvider: "claude";
}
