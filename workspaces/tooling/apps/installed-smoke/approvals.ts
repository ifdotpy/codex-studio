/** Read-only approval lifecycle witness. Run before either card is answered. */
import fs from "node:fs";
import {
  RequestStatus,
  TaskStatus,
} from "../../../client/apps/web/src/generated/api.ts";
import type { components } from "../../../client/apps/web/src/generated/api.ts";

type Schema = components["schemas"];
type Request = Required<
  Pick<Schema["RequestEntityDto"], "id" | "agent" | "method" | "status">
>;
type Monitor = Required<
  Pick<Schema["MonitorEntityDto"], "id" | "agent" | "status">
> &
  Pick<Schema["MonitorEntityDto"], "exitCode">;
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
  evidencePath,
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
  !evidencePath ||
  !Number.isFinite(duration) ||
  duration <= 0 ||
  acceptMonitor === declineMonitor ||
  acceptRequest === declineRequest ||
  acceptMarker === declineMarker ||
  evidencePath === acceptMarker ||
  evidencePath === declineMarker
) {
  throw new Error(
    "Usage: node approvals.ts <server-url> <agent-id> <accept-monitor-id> <accept-request-id> <accept-marker> <decline-monitor-id> <decline-request-id> <decline-marker> <deadline-seconds> <evidence-json-path>",
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
const sequences = new Map<string, number>();
const removed = new Set<string>();
const events: object[] = [];
let ready = false;
const startedAt = new Date().toISOString();

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
function requestStatus(value: unknown): value is Schema["RequestStatus"] {
  return Object.values(RequestStatus).some((status) => status === value);
}
function monitorStatus(value: unknown): value is Schema["TaskStatus"] {
  return Object.values(TaskStatus).some((status) => status === value);
}

function request(value: unknown, id: string): Request {
  if (
    !record(value) ||
    value.id !== id ||
    typeof value.agent !== "string" ||
    typeof value.method !== "string" ||
    !requestStatus(value.status)
  )
    throw new Error(`Invalid request: ${id}`);
  return {
    id,
    agent: value.agent,
    method: value.method,
    status: value.status,
  };
}
function monitor(value: unknown, id: string): Monitor {
  if (
    !record(value) ||
    value.id !== id ||
    typeof value.agent !== "string" ||
    !monitorStatus(value.status) ||
    (value.exitCode != null && typeof value.exitCode !== "number")
  )
    throw new Error(`Invalid monitor: ${id}`);
  return {
    id,
    agent: value.agent,
    status: value.status,
    exitCode: value.exitCode,
  };
}
async function get(route: string): Promise<unknown> {
  const response = await fetch(base + route, { signal });
  if (!response.ok) throw new Error(`${route}: HTTP ${response.status}`);
  return response.json();
}
function apply(doc: unknown, fromStream: boolean) {
  if (
    !record(doc) ||
    typeof doc.collection !== "string" ||
    typeof doc.id !== "string" ||
    typeof doc.seq !== "number" ||
    (doc.op !== "upsert" && doc.op !== "delete")
  )
    throw new Error("Invalid entity document");
  if (
    !targets.some(
      (t) =>
        (doc.collection === "request" && doc.id === t.requestId) ||
        (doc.collection === "monitor" && doc.id === t.monitorId),
    )
  )
    return;
  const key = `${doc.collection}:${doc.id}`;
  if (doc.seq <= (sequences.get(key) ?? 0)) return;
  sequences.set(key, doc.seq);
  if (doc.op === "delete") {
    if (doc.collection === "request") requests.delete(doc.id);
    else monitors.delete(doc.id);
    if (fromStream && ready && doc.collection === "request")
      removed.add(doc.id);
  } else {
    if (doc.collection === "request") {
      requests.set(doc.id, request(doc.value, doc.id));
      removed.delete(doc.id);
    } else monitors.set(doc.id, monitor(doc.value, doc.id));
  }
  if (fromStream)
    events.push({
      collection: doc.collection,
      id: doc.id,
      op: doc.op,
      seq: doc.seq,
      value: doc.op === "upsert" ? doc.value : undefined,
    });
}
async function baseline() {
  let cursor = 0;
  for (;;) {
    const page = await get(
      `/api/sync/pull?scope=state:entities:v1&after=${cursor}&limit=500`,
    );
    if (
      !record(page) ||
      !record(page.checkpoint) ||
      typeof page.checkpoint.seq !== "number" ||
      typeof page.maxSeq !== "number" ||
      !Array.isArray(page.documents) ||
      page.checkpoint.seq < cursor
    )
      throw new Error("Invalid entity checkpoint");
    for (const doc of page.documents) apply(doc, false);
    cursor = page.checkpoint.seq;
    if (cursor >= page.maxSeq || page.documents.length === 0) return;
  }
}
function pending(t: (typeof targets)[number]) {
  const request = requests.get(t.requestId);
  const monitor = monitors.get(t.monitorId);
  return (
    request !== undefined &&
    monitor !== undefined &&
    request.agent === agent &&
    request.method === "monitor/approve" &&
    request.status === RequestStatus.pending &&
    monitor.agent === agent &&
    monitor.status === TaskStatus.approval
  );
}
function markerLines(file: string) {
  try {
    return fs.readFileSync(file, "utf8").split("\n").filter(Boolean).length;
  } catch (error) {
    if (record(error) && error.code === "ENOENT") return 0;
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
    const expected =
      t.decision === "accept" ? TaskStatus.completed : TaskStatus.cancelled;
    if (!removed.has(t.requestId) || requests.has(t.requestId))
      throw new Error(
        `${t.decision}: approval request removal was not witnessed over SSE`,
      );
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
}
function consume(frame: string) {
  const lines = frame.split("\n");
  if (lines.includes("event: entity-transfer"))
    throw new Error(
      "SSE entity transfer requires recovery; evidence incomplete",
    );
  if (!lines.includes("event: resources")) return;
  const data = lines
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trimStart())
    .join("\n");
  const event: unknown = JSON.parse(data);
  if (
    !record(event) ||
    !Array.isArray(event.resourceVersions) ||
    typeof event.reason !== "string"
  )
    throw new Error("Invalid SSE resources event");
  if (["overflow", "reconnect", "workspace"].includes(event.reason))
    throw new Error(`SSE ${event.reason}: evidence incomplete`);
  for (const entry of event.resourceVersions) {
    if (!record(entry)) throw new Error("Invalid SSE resource entry");
    if (entry.entityChanges == null) continue;
    if (
      !record(entry.entityChanges) ||
      !Array.isArray(entry.entityChanges.documents)
    )
      throw new Error("Invalid SSE entity changes");
    for (const doc of entry.entityChanges.documents) apply(doc, true);
  }
  if (!ready && targets.every(pending)) {
    ready = true;
    console.log(
      "READY: both exact approval cards are pending; user may decide",
    );
  }
}
async function run() {
  const protocol = await get("/api/sync/protocol");
  if (!record(protocol) || typeof protocol.protocolVersion !== "number")
    throw new Error("Invalid sync protocol");
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
  await baseline();
  if (targets.every(pending)) {
    ready = true;
    console.log(
      "READY: both exact approval cards are pending; user may decide",
    );
  }
  let buffer = "";
  const decoder = new TextDecoder();
  try {
    for await (const chunk of stream.body) {
      buffer += decoder.decode(chunk, { stream: true });
      buffer = buffer.replaceAll("\r\n", "\n");
      let end: number;
      while ((end = buffer.indexOf("\n\n")) >= 0) {
        consume(buffer.slice(0, end));
        buffer = buffer.slice(end + 2);
      }
    }
    throw new Error("SSE closed before the deadline");
  } catch (error) {
    if (error !== signal.reason || !signal.aborted) throw error;
  }
  verify();
}
let outcome = "passed";
let failure: string | undefined;
try {
  await run();
} catch (error) {
  outcome = "failed";
  failure = error instanceof Error ? error.message : String(error);
  process.exitCode = 1;
}
fs.writeFileSync(
  evidencePath,
  JSON.stringify(
    {
      startedAt,
      finishedAt: new Date().toISOString(),
      server: base,
      agent,
      targets,
      ready,
      events,
      markerCounts: targets.map((t) => ({
        decision: t.decision,
        lines: markerLines(t.marker),
      })),
      outcome,
      failure,
    },
    null,
    2,
  ) + "\n",
  { flag: "wx" },
);
if (failure) console.error(`FAIL: ${failure}`);
else
  console.log(
    "PASS: both cards resolved; accepted marker has one line; declined marker is absent or empty",
  );
