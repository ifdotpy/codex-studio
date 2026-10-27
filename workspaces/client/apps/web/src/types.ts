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

export type {
  JsonValue,
  JsonInput,
  JsonInputOf,
  Json,
  KnownApiFields,
  TranscriptWireItem,
  TranscriptPageData,
} from "./apiTypes";
import type { JsonInput, TranscriptWireItem } from "./apiTypes";
export type Agent = AgentEntityDto;
export type Chat = ChatEntityDto;
export type PeerTeam = PeerTeamEntityDto;
export type Room = RoomEntityDto;
export type Complaint = ComplaintEntityDto;
export type BackgroundTask = TaskEntityDto;
export type WorkspaceTask = GetResult<"/api/workspace/tasks">["tasks"][number];
export type Monitor = MonitorEntityDto;
export type Request = RequestEntityDto;
export type Rule = RuleEntityDto;
export type Project = ProjectEntityDto;
export type Event = EventEntityDto;
export type Work = WorkEntityDto;
export type Edge = EdgeEntityDto;

type SnapshotRuntime = WorkspaceEntityDto & {
  modelsByAccount?:
    | Record<string, components["schemas"]["ModelEntityDto"][]>
    | undefined;
  terminals?: components["schemas"]["TerminalRecord"][] | undefined;
  desktop?: components["schemas"]["DesktopResponse"] | undefined;
  sessionCostsByAgent?:
    | Record<string, components["schemas"]["SessionCostResponse"]>
    | undefined;
  accountCostsByAccount?:
    | Record<string, components["schemas"]["AccountCostResponse"]>
    | undefined;
  accountsSummary?: components["schemas"]["AccountsSummaryDto"] | undefined;
  queuesByAgent?:
    | Record<string, components["schemas"]["QueueView"]>
    | undefined;
  rateLimitsByAccount?:
    | Record<string, components["schemas"]["AccountRateLimitsDto"]>
    | undefined;
  rateLimits?: components["schemas"]["AccountRateLimitsDto"] | null | undefined;
  retainedWorktrees?:
    | components["schemas"]["RetainedWorktreeEntityDto"][]
    | undefined;
  questionHistory?: components["schemas"]["AgentRequest"][] | undefined;
  messageReceipts?:
    | components["schemas"]["MessageReceiptEntityDto"][]
    | undefined;
  plans?: components["schemas"]["PlanSummaryEntityDto"][] | undefined;
  annotations?: components["schemas"]["AnnotationEntityDto"][] | undefined;
  studioErrors: components["schemas"]["StudioError"][];
  agents: Agent[];
  rooms: Room[];
  tasks: BackgroundTask[];
  monitors: Monitor[];
  complaints: Complaint[];
  requests: Request[];
  rules: Rule[];
  projects: Project[];
  /** Project entity sequence by native ID, owned by the entity projection; stale-delete guard. */
  projectSequences?: Record<string, number> | undefined;
  peerTeams: PeerTeam[];
  events: Event[];
  work: Work[];
};

/** Fields assembled for renderer consumers from entity collections. */
type ProjectionSnapshotFields = {
  /** Workspace of the applied entity rows, stamped by their projection handle. */
  entityWorkspaceId?: string | undefined;
  token: string;
  stateDir: string;
  threads: Agent[];
  chats: Chat[];
  nodes: Array<Agent | Chat>;
  edges: Edge[];
  /** False while the first sync pull of this projection still runs: records can be missing. */
  entitiesComplete?: boolean | undefined;
};

export type Snapshot = ProjectionSnapshotFields & { runtime: SnapshotRuntime };
export type FederationPeer = components["schemas"]["FederationPeer"];
export type FederationRoom = components["schemas"]["FederationRoom"];
export type FederationSnapshot = components["schemas"]["FederationSnapshot"];

type TranscriptAsset = NonNullable<TranscriptWireItem["assets"]>[number];
export type LocalMessageAttachment = {
  id: string;
  name: string;
  mime: string;
  image: boolean;
  size: number;
  preview?: string | undefined;
};
export type Message = Omit<
  TranscriptWireItem,
  "assets" | "deliveryStatus" | "nativeError" | "role" | "text" | "threadId"
> & {
  assets?: Array<TranscriptAsset | LocalMessageAttachment> | null | undefined;
  deliveryStatus?:
    | TranscriptWireItem["deliveryStatus"]
    | "sending"
    | "paused"
    | undefined;
  nativeError?: JsonInput | undefined;
  role: NonNullable<TranscriptWireItem["role"]>;
  text: NonNullable<TranscriptWireItem["text"]>;
  accountKey?: string | undefined;
  author?: string | undefined;
  effort?: string | undefined;
  excerpt?: string | undefined;
  localDelivery?: boolean | undefined;
  model?: string | undefined;
  title?: string | undefined;
  pending?: boolean | undefined;
  senderName?: string | undefined;
  sender?: string | undefined;
  seq?: number | undefined;
  created?: number | undefined;
  threadId?: string | undefined;
  timestamp?: number | string | undefined;
};
export const busy = new Set(["running", "starting", "approval"]);
export const nativeReleaseLabel = (agent: Agent) => {
  const release = agent.nativeRelease;
  if (agent.inFlight || !release || release.phase !== "released") return null;
  return release.resetPending
    ? "Tool reset waits for Codex to close the native session"
    : "Native thread released";
};
export function statusLabel(
  status: Agent["status"],
  phase?: string,
  parkedEvent?: string,
): string {
  switch (status) {
    case null:
    case undefined:
      return "";
    case "running":
      switch (phase) {
        case "thinking":
          return "Thinking";
        case "writing":
          return "Writing";
        case "tool":
          return "Using tools";
        case "retrying":
          return "Codex is retrying";
        case "auth":
          return "Restoring sign-in";
        case "error":
          return "Codex reported an error";
      }
      return "Working";
    case "idle":
      return "Ready";
    case "starting":
      return "Starting";
    case "queued":
      return "Queued";
    case "waiting":
      return "Turn ended";
    case "parked":
      return parkedEvent ? `Waiting for event ${parkedEvent}` : "Turn ended";
    case "waiting_on_monitor":
      return "Waiting on command";
    case "completed":
      return "Complete";
    case "failed":
      return "Failed";
    case "paused":
      return "Stopped";
    case "approval":
      return "Needs an answer";
    case "interrupted":
      return "Interrupted";
    case "unknown":
    case "blocked":
    case "abandoned":
    case "capacity-retry":
      return status;
  }
  return status satisfies never;
}
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
