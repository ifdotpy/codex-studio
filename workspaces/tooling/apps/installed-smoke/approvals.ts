/** Read-only approval lifecycle witness. Run before either card is answered. */
import fs from "node:fs";
import { RequestStatus } from "../../../client/apps/web/src/generated/api.ts";
import type { components } from "../../../client/apps/web/src/generated/api.ts";

type Schema = components["schemas"];
type Entity = Schema["SyncEntity"];
type Request = Schema["RequestEntityDto"];
type Monitor = Schema["MonitorEntityDto"];

const [
  url,
  agent,
  acceptMonitor,
  acceptRequest,
  acceptMarker,
  declineMonitor,
  declineRequest,
  declineMarker,
  seconds,
] = process.argv.slice(2);
const duration = Number(seconds);
if (
  !url ||
  !agent ||
  !acceptMonitor ||
  !acceptRequest ||
  !acceptMarker ||
  !declineMonitor ||
  !declineRequest ||
  !declineMarker ||
  !Number.isFinite(duration) ||
  duration <= 0 ||
  acceptMonitor === declineMonitor ||
  acceptRequest === declineRequest ||
  acceptMarker === declineMarker
) {
  throw new Error(
    "Usage: node approvals.ts <server-url> <agent-id> <accept-monitor-id> <accept-request-id> <accept-marker> <decline-monitor-id> <decline-request-id> <decline-marker> <deadline-seconds>",
  );
}
const targets = [
  {
    monitorId: acceptMonitor,
    requestId: acceptRequest,
    marker: acceptMarker,
    decision: "accept",
  },
  {
    monitorId: declineMonitor,
    requestId: declineRequest,
    marker: declineMarker,
    decision: "decline",
  },
] as const;
const signal = AbortSignal.timeout(duration * 1000);
const base = url.replace(/\/$/, "");
const requests = new Map<string, Request>();
const monitors = new Map<string, Monitor>();
const removed = new Set<string>();
let cursor = 0;
let ready = false;
let pulling = false;

async function get<T>(route: string): Promise<T> {
  const response = await fetch(base + route, { signal });
  if (!response.ok) throw new Error(`${route}: HTTP ${response.status}`);
  return (await response.json()) as T;
}
function apply(doc: Entity) {
  if (
    doc.collection === "request" &&
    targets.some((t) => t.requestId === doc.id)
  ) {
    if (doc.op === "delete") {
      requests.delete(doc.id);
      removed.add(doc.id);
    } else {
      requests.set(doc.id, doc.value);
      removed.delete(doc.id);
    }
  }
  if (
    doc.collection === "monitor" &&
    targets.some((t) => t.monitorId === doc.id)
  ) {
    if (doc.op === "delete") monitors.delete(doc.id);
    else monitors.set(doc.id, doc.value);
  }
}
async function pull() {
  pulling = true;
  for (;;) {
    const page = await get<Schema["SyncPullResponse"]>(
      `/api/sync/pull?scope=state:entities:v1&after=${cursor}&limit=500`,
    );
    for (const doc of page.documents) {
      if (!("collection" in doc))
        throw new Error("Unexpected non-entity document");
      apply(doc);
    }
    if (page.maxSeq == null || page.checkpoint.seq < cursor)
      throw new Error("Invalid entity checkpoint");
    cursor = page.checkpoint.seq;
    if (cursor >= page.maxSeq || page.documents.length === 0) break;
  }
  pulling = false;
}
function pending(t: (typeof targets)[number]) {
  const request = requests.get(t.requestId);
  const monitor = monitors.get(t.monitorId);
  return (
    request !== undefined &&
    monitor !== undefined &&
    request.agent === agent &&
    request.method === "monitor/approve" &&
    request.params?.monitorId === t.monitorId &&
    request.status === RequestStatus.pending &&
    monitor.agent === agent &&
    monitor.status === "approval"
  );
}
function markerLines(file: string) {
  try {
    return fs.readFileSync(file, "utf8").split("\n").filter(Boolean).length;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return 0;
    throw error;
  }
}
function verify() {
  if (!ready)
    throw new Error(
      "Did not observe both exact pending approval cards before their decisions",
    );
  for (const t of targets) {
    const monitor = monitors.get(t.monitorId);
    const expected = t.decision === "accept" ? "completed" : "cancelled";
    if (!removed.has(t.requestId) || requests.has(t.requestId))
      throw new Error(`${t.decision}: approval request was not removed`);
    if (
      monitor === undefined ||
      monitor.agent !== agent ||
      monitor.status !== expected ||
      (t.decision === "accept" && monitor.exitCode !== 0)
    )
      throw new Error(
        `${t.decision}: monitor did not reach ${expected} successfully`,
      );
    const count = markerLines(t.marker);
    if (count !== (t.decision === "accept" ? 1 : 0))
      throw new Error(`${t.decision}: marker has ${count} nonempty lines`);
  }
  console.log(
    "PASS: both cards resolved; accepted marker has one line; declined marker is absent or empty",
  );
}

const protocol =
  await get<Schema["SyncProtocolResponse"]>("/api/sync/protocol");
if (targets.some((target) => markerLines(target.marker) !== 0))
  throw new Error("Marker files must be absent or empty before decisions");
const resources: Schema["ResourceRef"][] = [
  { kind: "state" },
  { kind: "monitors", agentId: agent },
];
const stream = await fetch(
  `${base}/api/sync/stream?protocol=${protocol.protocolVersion}&resources=${encodeURIComponent(JSON.stringify(resources))}`,
  { signal, headers: { accept: "text/event-stream" } },
);
if (!stream.ok || !stream.body) throw new Error(`SSE HTTP ${stream.status}`);
try {
  await pull();
  if (targets.every(pending)) {
    ready = true;
    console.log(
      "READY: both exact approval cards are pending; user may decide",
    );
  }
  let buffer = "";
  const decoder = new TextDecoder();
  for await (const chunk of stream.body) {
    buffer += decoder.decode(chunk, { stream: true });
    buffer = buffer.replaceAll("\r\n", "\n");
    let end: number;
    while ((end = buffer.indexOf("\n\n")) >= 0) {
      const frame = buffer.slice(0, end);
      buffer = buffer.slice(end + 2);
      if (!frame.split("\n").includes("event: resources")) continue;
      await pull();
      if (!ready && targets.every(pending)) {
        ready = true;
        console.log(
          "READY: both exact approval cards are pending; user may decide",
        );
      }
    }
  }
  throw new Error("SSE closed before the deadline");
} catch (error) {
  if (!signal.aborted || pulling) throw error;
  verify();
}
