// The app-server publishes extensible JSON objects for tool and approval payloads.
export type Json = Record<string, any>;
export interface Agent extends Json {
  id: string;
  name: string;
  isLead?: boolean;
  rootId?: string;
  parentId?: string;
  source: string;
  status: string;
  cwd?: string;
  model: string;
  accountKey?: string;
  dangerouslySkipAccountRules?: boolean;
  yoloMode?: boolean | null;
  canSend?: boolean;
  threadId?: string;
  empty?: boolean;
  tail?: string;
  created: number;
  inFlight?: boolean;
  compactions?: number;
  panelVersion?: number;
  panelDataVersion?: number;
  contextUsage?: { tokens: number | null; window: number | null; at: number };
}
export interface Room {
  id: string;
  name: string;
  kind: "private" | "broadcast";
  rootId?: string;
  members: string[];
  updated: number;
  lastMessage?: { seq: number; text: string; sender: string; created: number };
}
export interface Complaint extends Json {
  recipient?: "user" | "lead";
  version?: number;
  id: string;
  leadId: string;
  author: string;
  authorName: string;
  leadName: string;
  title: string;
  status: string;
  needsResponse: boolean;
  created: number;
  readAt: number | null;
  leadStopped: boolean;
  leadDeleted: boolean;
}
export const complaintNeedsUserResponse = (complaint: Complaint) =>
  complaint.recipient
    ? complaint.recipient === "user" && complaint.needsResponse
    : complaint.author === complaint.leadId;

export interface BackgroundTask {
  id: string;
  agent: string;
  kind: "monitor" | "command" | "tool";
  status: string;
  created: number;
  finished?: number;
  name?: string;
  command?: string;
  query?: string;
  cwd?: string;
  processId?: string;
  durationMs?: number;
  timeout_ms?: number;
  interactive?: boolean;
  stdinClosed?: boolean;
  stdinCloseRequested?: string;
  stdinError?: string;
  cancelRequested?: boolean;
  exitCode?: number | null;
  arguments?: string;
  tail?: string;
  error?: string;
  bytes?: number;
  log?: string;
  outputTruncated?: boolean;
}
export interface Snapshot {
  token: string;
  stateDir: string;
  threads: Agent[];
  chats: Json[];
  runtime: {
    agents: Agent[];
    projects?: { id: string; path: string; name: string; created: number }[];
    rooms: Room[];
    complaints: Complaint[];
    monitors: Json[];
    tasks?: BackgroundTask[];
    work?: Json[];
    userTasks?: UserTask[];
    rules?: Json[];
    tasksHistoryLimit?: number;
    requests: Json[];
    events?: Json[];
    rateLimits?: Json;
    rateLimitsByAccount?: Record<string, Json>;
  };
}
export interface Message extends Json {
  id: string;
  role: string;
  text: string;
  title?: string;
  pending?: boolean;
  senderName?: string;
  sender?: string;
  seq?: number;
  created?: number;
}
export const busy = new Set(["running", "starting", "approval"]);
export const statusLabel = (status: string, phase?: string) =>
  (status === "running" &&
    phase &&
    (
      {
        thinking: "Thinking",
        writing: "Writing",
        tool: "Using tools",
      } as Record<string, string>
    )[phase]) ||
  {
    idle: "Ready",
    running: "Working",
    starting: "Starting",
    queued: "Queued",
    waiting: "Waiting for results",
    completed: "Complete",
    failed: "Failed",
    paused: "Stopped",
    approval: "Needs an answer",
    interrupted: "Interrupted",
  }[status] ||
  status;
export const complaintLabel = (status: string) =>
  ({
    open: "Awaiting response",
    in_progress: "In progress",
    resolved: "Resolved",
    declined: "Declined",
  })[status] || status;

export interface UserTask extends Json {
  id: string;
  agent: string;
  rootId: string;
  title: string;
  description: string;
  criteria: string;
  status: "open" | "review" | "accepted" | "cancelled";
  version: number;
  created: number;
  updated: number;
  history: { action: string; text: string; actor: string; at: number }[];
  reason: string;
  completionNote: string;
}
