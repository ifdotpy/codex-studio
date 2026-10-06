import { nativeErrorView } from "./nativeErrors";
import type { GetResult } from "./api";
import type {
  AgentEntityDto,
  ChatEntityDto,
  ComplaintEntityDto,
  EdgeEntityDto,
  EventEntityDto,
  MonitorEntityDto,
  PeerTeamEntityDto,
  ProjectEntityDto,
  RequestEntityDto,
  RoomEntityDto,
  RuleEntityDto,
  TaskEntityDto,
  WorkEntityDto,
  WorkspaceEntityDto,
  components,
} from "./generated/api";

// Provider and extensible app-server payloads are JSON objects, not untyped maps.
export type JsonValue = components["schemas"]["JsonValue"];
export type Json = Record<string, JsonValue>;
export type Agent = AgentEntityDto;
export type Chat = ChatEntityDto;
export type PeerTeam = PeerTeamEntityDto;
export type Room = RoomEntityDto;
export type Complaint = ComplaintEntityDto;
export type BackgroundTask = TaskEntityDto;
export type WorkspaceTask = GetResult<"/api/workspace/tasks">["tasks"][number];
export type ProjectedMonitorTask = Monitor & { kind: "monitor" };
type RendererTaskFields = {
  agent: string;
  created: number;
  kind: string;
  status: string;
};
export type DisplayBackgroundTask = (
  | BackgroundTask
  | WorkspaceTask
  | ProjectedMonitorTask
) &
  RendererTaskFields;
export type Monitor = MonitorEntityDto;
export type Request = RequestEntityDto;
export type Rule = RuleEntityDto;
export type Project = ProjectEntityDto;
export type Event = EventEntityDto;
export type Work = WorkEntityDto;
export type Edge = EdgeEntityDto;

type SnapshotRuntime = WorkspaceEntityDto & {
  agents: Agent[];
  rooms: Room[];
  tasks: BackgroundTask[];
  monitors: Monitor[];
  complaints: Complaint[];
  requests: Request[];
  rules: Rule[];
  projects: Project[];
  peerTeams: PeerTeam[];
  events: Event[];
  work: Work[];
};

/** Fields assembled for renderer consumers from entity collections. */
type ProjectionSnapshotFields = {
  token: string;
  stateDir: string;
  threads: Agent[];
  chats: Chat[];
  nodes: Array<Agent | Chat>;
  edges: Edge[];
};

export type Snapshot = ProjectionSnapshotFields & { runtime: SnapshotRuntime };
export type LegacySnapshot = GetResult<"/api/state">;
export type FederationPeer = components["schemas"]["FederationPeer"];
export type FederationRoom = components["schemas"]["FederationRoom"];
export type FederationSnapshot = components["schemas"]["FederationSnapshot"];

type TranscriptWireItem = GetResult<"/api/transcript">["items"][number];
type TranscriptAsset = NonNullable<TranscriptWireItem["assets"]>[number];
export type LocalMessageAttachment = {
  id: string;
  name: string;
  mime: string;
  image: boolean;
  size: number;
  preview?: string;
};
export type Message = Omit<
  TranscriptWireItem,
  "assets" | "deliveryStatus" | "role" | "text"
> & {
  assets?: Array<TranscriptAsset | LocalMessageAttachment> | null;
  deliveryStatus?: TranscriptWireItem["deliveryStatus"] | "sending" | "paused";
  role: NonNullable<TranscriptWireItem["role"]>;
  text: NonNullable<TranscriptWireItem["text"]>;
  accountKey?: string;
  author?: string;
  details?: JsonValue;
  effort?: string;
  excerpt?: string;
  localDelivery?: boolean;
  model?: string;
  nativeError?: JsonValue;
  nativeHook?: boolean;
  nativeHookQuiet?: boolean;
  nativeNotice?: "error" | "warning";
  nativeReview?: boolean;
  title?: string;
  pending?: boolean;
  senderName?: string;
  sender?: string;
  seq?: number;
  created?: number;
  threadId?: string;
  timestamp?: number | string;
};
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
    typeof agent.status === "string" &&
    ["failed", "interrupted", "paused"].includes(agent.status) &&
    !agent.inFlight
  )
    return "The previous attempt stopped because Codex was offline.";
  return nativeErrorView(agent.error).message;
}

export function agentStopReason(agent: Agent): string {
  if (
    agent.status !== "paused" ||
    agent.autoWake !== false ||
    typeof agent.error !== "string"
  )
    return "";
  const reason = agent.error;
  return reason === "Stopped by user" ||
    (reason.startsWith("Stopped by agent ") &&
      reason.slice("Stopped by agent ".length).trim())
    ? reason
    : "";
}
