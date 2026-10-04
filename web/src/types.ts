import { nativeErrorView } from "./nativeErrors";
import type { GetResult } from "./api";
import type { components } from "./generated/api";

// Provider and extensible app-server payloads are JSON objects, not untyped maps.
export type JsonValue = components["schemas"]["JsonValue"];
export type Json = Record<string, JsonValue>;
export type Agent = components["schemas"]["SnapshotAgentDto"];
export type Snapshot = GetResult<"/api/state">;
export type PeerTeam = components["schemas"]["PeerTeamEntityDto"];
export type Room = NonNullable<Snapshot["runtime"]>["rooms"][number];
export type Complaint = NonNullable<Snapshot["runtime"]>["complaints"][number];
export type BackgroundTask = NonNullable<Snapshot["runtime"]>["tasks"][number];
export type FederationPeer = components["schemas"]["FederationPeer"];
export type FederationRoom = components["schemas"]["FederationRoom"];
export type FederationSnapshot = components["schemas"]["FederationSnapshot"];

export const complaintNeedsUserResponse = (complaint: Complaint) =>
  complaint.recipient
    ? complaint.recipient === "user" && complaint.needsResponse
    : complaint.author === complaint.leadId;

type TranscriptWireItem = GetResult<"/api/transcript">["items"][number];
export type Message = TranscriptWireItem & {
  role: NonNullable<TranscriptWireItem["role"]>;
  text: NonNullable<TranscriptWireItem["text"]>;
  title?: string;
  pending?: boolean;
  senderName?: string;
  sender?: string;
  seq?: number;
  created?: number;
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
    ["failed", "interrupted", "paused"].includes(agent.status) &&
    !agent.inFlight
  )
    return "The previous attempt stopped because Codex was offline.";
  return nativeErrorView(agent.error).message;
}
