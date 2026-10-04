import { nativeErrorView } from "./nativeErrors";
import type {
  AgentEntityDto,
  EventEntityDto,
  MonitorEntityDto,
  TaskEntityDto,
} from "./generatedEntityTypes";
// The app-server publishes extensible JSON objects for tool and approval payloads.
export type Json = Record<string, any>;
export interface Agent extends Json, AgentEntityDto {
  id: string;
  name: string;
  isLead?: boolean;
  sharedRoomId?: string;
  rootId?: string;
  parentId?: string;
  source: string;
  status: string;
  error?: unknown;
  cwd?: string;
  model: string;
  concurrency?: number;
  subagentConcurrencyVersion?: number;
  occupied?: number;
  queued?: number;
  agentMode?: "multi" | "single";
  agentModeRevision?: number;
  agentModeSupported?: boolean;
  accountKey?: string;
  yoloMode?: boolean | null;
  canSend?: boolean;
  threadId?: string;
  empty?: boolean;
  tail?: string;
  created: number;
  inFlight?: boolean;
  parkedEvent?: string;
  worktreeDisk?: { state: string; bytes?: number; scannedAt?: number };
  compactions?: number;
  panelVersion?: number;
  panelDataVersion?: number;
  contextUsage?: { tokens: number | null; window: number | null; at: number };
}
export interface PeerTeam {
  id: string;
  name: string;
  projectPath: string;
  members: string[];
}
export interface Room {
  userHidden?: boolean;
  projectPath?: string;
  radio?: {
    direct?: boolean;
    teamId: string;
    revision: number;
    status: "idle" | "waiting" | "speaking" | "stopping" | "blocked";
    speaker: string | null;
    next: string[];
    active: { eventId: string; agentId: string; turnId?: string } | null;
    error: string | null;
  };
  peerTeamId?: string;
  peerTeamName?: string;
  id: string;
  name: string;
  kind: "private" | "broadcast" | "federated";
  rootId?: string;
  members: string[];
  federated?: boolean;
  peerId?: string;
  peerLabel?: string;
  localMembers?: string[];
  remoteMembers?: {
    id: string;
    name?: string;
    role?: string;
    status?: string;
  }[];
  updated: number;
  lastMessage?: { seq: number; text: string; sender: string; created: number };
}
export interface FederationPeer {
  stateId: string;
  label: string;
  origin: string;
  publicKey: string;
  status: string;
  localApproved: boolean;
  remoteApproved: boolean;
  whoisStatus?: string;
  whoisUser?: string | null;
  created?: number;
  updated?: number;
  lastError?: string;
}
export interface FederationRoom {
  id: string;
  peerId: string;
  peerLabel: string;
  name: string;
  status: string;
  localMembers: string[];
  remoteMembers: {
    id: string;
    name?: string;
    role?: string;
    status?: string;
  }[];
  shareNames: boolean;
  shareStatus: boolean;
  created?: number;
}
export interface FederationSnapshot {
  version: number;
  enabled: boolean;
  identity: { stateId: string; label: string; fingerprint: string } | null;
  peers: FederationPeer[];
  invites: {
    id: string;
    created: number;
    expires: number;
    status: string;
    expectedUser?: string;
  }[];
  rooms: FederationRoom[];
  queued: number;
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

export interface BackgroundTask extends TaskEntityDto {
  id: string;
  turnId?: string;
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
    projectOrganizationVersion?: number;
    sidebarOrder?: {
      revision: number;
      groups: Record<string, string[]> | null;
    };
    peerTeamsVersion?: number;
    peerTeams?: PeerTeam[];
    federation?: FederationSnapshot;
    agents: Agent[];
    projects?: {
      id: string;
      path: string;
      name: string;
      created: number;
      accountKey?: string;
      accountRevision?: number;
      accountKeys?: string[];
      workerBaseRef?: string | null;
      workerBaseRevision?: number;
      organizationRevision?: number;
      peerTeamsRevision?: number;
      folders?: { id: string; name: string; parentId: string | null }[];
    }[];
    rooms: Room[];
    complaints: Complaint[];
    monitors: (Omit<MonitorEntityDto, "id" | "agent" | "status" | "created"> &
      Json & {
        id: string;
        agent: string;
        status: string;
        created: number;
      })[];
    tasks?: BackgroundTask[];
    work?: Json[];
    rules?: Json[];
    tasksHistoryLimit?: number;
    requests: Json[];
    events?: (Omit<EventEntityDto, "id"> & { id: string })[];
    rateLimits?: Json;
    rateLimitsByAccount?: Record<string, Json>;
    nativeNotices?: Json[];
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
export const nativeReleaseLabel = (agent: Agent) => {
  const release = agent.nativeRelease;
  if (agent.inFlight || !release || release.phase !== "released") return null;
  return release.resetPending
    ? "Tool reset waits for Codex to close the native session"
    : "Native thread released";
};
export const statusLabel = (
  status: string,
  phase?: string,
  parkedEvent?: string,
) =>
  (status === "parked" && parkedEvent
    ? `Waiting for event ${parkedEvent}`
    : null) ||
  (status === "running" &&
    phase &&
    (
      {
        thinking: "Thinking",
        writing: "Writing",
        tool: "Using tools",
        retrying: "Codex is retrying",
        auth: "Restoring sign-in",
        error: "Codex reported an error",
      } as Record<string, string>
    )[phase]) ||
  {
    idle: "Ready",
    running: "Working",
    starting: "Starting",
    queued: "Queued",
    waiting: "Turn ended",
    completed: "Complete",
    failed: "Failed",
    paused: "Stopped",
    parked: "Turn ended",
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

// A stored turn failure is not the current health of the account connection.
export function agentErrorLabel(agent: Agent): string {
  if (
    agent.error === "Codex app-server is offline" &&
    ["failed", "interrupted", "paused"].includes(agent.status) &&
    !agent.inFlight
  )
    return "The previous attempt stopped because Codex was offline.";
  return nativeErrorView(agent.error).message;
}
