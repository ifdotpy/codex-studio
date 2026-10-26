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
  target?: string;
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
    backup?: string;
  }[];
}
export interface TurnError {
  resetsAt?: number | undefined;
  message: string;
  codexErrorInfo?: string;
  data?: Record<string, unknown>;
}
export interface Item {
  id?: string;
  type?: string;
  nativeId?: string | undefined;
  nativeIds?: string[];
  nativeSubmissionAttemptId?: string;
  delivery?: string;
  deliveryStatus?: string;
  content?: unknown;
  text?: string;
  thinking?: string | boolean;
  nativeSubagent?: boolean;
  canMoveToBackground?: boolean;
  backgroundTaskId?: string;
  backgroundedByUser?: boolean;
  outputFile?: string;
  stalled?: boolean;
  compactionMetadata?: import("@anthropic-ai/claude-agent-sdk").SDKCompactBoundaryMessage["compact_metadata"];
  clientId?: string;
  elapsedSeconds?: number;
  phase?: string;
  studioNotice?: boolean;
  error?: string;
  status?: string;
  tool?: string;
  arguments?: Record<string, unknown>;
  command?: string;
  cwd?: string;
  aggregatedOutput?: string;
  exitCode?: number | null;
  result?: unknown;
  server?: string;
  changes?: { path: string; kind: { type: string }; diff: string }[];
}
export interface Turn {
  synthetic?: boolean;
  id: string;
  clientUserMessageId?: string;
  status?: string;
  startOutcome?: string;
  items: Item[];
  nativeSessionId?: string;
  nativeHistoryDetached?: boolean;
  nativeMessageId?: string;
  fileCheckpointMessageId?: string;
  fileCheckpointSource?: { nativeId: string; cwd: string };
  idleCompaction?: boolean;
  interrupted?: boolean;
  error?: TurnError;
  stallError?: TurnError;
  limitError?: { codexErrorInfo: string; resetsAt?: number };
  apiErrorInfo?: string;
}
export interface ControlReceipt {
  kind: "rollback";
  turnId: string;
  restoreFiles?: boolean;
  fileMessageId?: string | undefined;
  status: "submitted" | "forked" | "rewind_submitted" | "rewound" | "completed";
  sourceNativeId: string;
  targetNativeId?: string;
  createdAt: string;
  retainedCount: number;
  sourceMessages?: SessionMessage[];
  previousTurns?: Turn[];
  upToMessageId: string | null;
  filePlan?: FileRestorePlan | undefined;
  fileResult?: RewindFilesResult | undefined;
  fileBackupReleaseAuthorized?: boolean;
  fileBackupReleased?: boolean;
  result?: {
    threadId: string;
    nativeId: string;
    removedTurns: number;
    filesRestored?: boolean;
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
  started?: boolean;
  ephemeral?: boolean;
  pendingNativeSession?: PendingTransition;
  nativeSessionTransitionError?: string;
  nativeSessionTransitions?: {
    from: string;
    to: string;
    source: TransitionSource;
  }[];
  controlRequests?: Record<string, ControlReceipt>;
  historyBranches?: { requestId: string; nativeId: string; turns: Turn[] }[];
  journalRepair?: JournalRepairReport;
  createdAt?: number;
  updatedAt?: number;
  preview?: string;
  name?: string | null;
  model?: Options["model"];
  approvalPolicy?: string;
  activePermissionProfile?: unknown;
  sandbox?: string;
  claude?: import("./thinking.ts").ClaudeSettings;
  developerInstructions?: string;
  dynamicTools?: ToolDefinition[];
  moveProof?: import("./bridge-protocol.ts").BridgeMoveProof;
  dynamicToolsJson?: string;
  capabilityProfile?: unknown;
  moveTurnOptions?: Partial<Options>;
  userMcp?: boolean;
  studioImageWorkspaceTempDir?: string;
  claudeVersion?: string;
  idleCache?: CacheReport;
  contextWindow?: number;
  totalTokens?: number;
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
  journalRepair?: JournalRepairReport;
  status: { type: "active" | "idle" };
  modelProvider: "claude";
}
