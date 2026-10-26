#!/usr/bin/env node
// Offline replay only: no server reads or writes. Payloads stay outside the checkout.
import { readFile, writeFile } from "node:fs/promises";
import type { components } from "../../../client/apps/web/src/generated/api.ts";
import type { OpenAPI3 } from "openapi-typescript";
import {
  createBrotliCompress,
  brotliDecompressSync,
  constants,
} from "node:zlib";

const [input, output] = process.argv.slice(2);
if (!input || !output)
  throw new Error("usage: replay-view-scoped.ts <recorded.sse> <report.json>");
const schema: OpenAPI3 = JSON.parse(
  await readFile(
    new URL(
      "../../../runtime/packages/runtime-http/openapi.json",
      import.meta.url,
    ),
    "utf8",
  ),
);
const source = await readFile(
  new URL(
    "../../../runtime/packages/runtime-http/src/routes/sync_stream.rs",
    import.meta.url,
  ),
  "utf8",
);
const encoderValue = (name: string) => {
  const value = source.match(new RegExp(`const ${name}: u32 = (\\d+);`))?.[1];
  if (!value) throw new Error(`Missing stream encoder owner ${name}`);
  return Number(value);
};
const quality = encoderValue("STREAM_BROTLI_QUALITY");
const lgwin = encoderValue("STREAM_BROTLI_LGWIN");
const schemas = schema.components?.schemas;
if (!schemas) throw new Error("Missing schema components");
const summary = schemas.AgentSummaryEntityDto;
if (!summary || !("properties" in summary))
  throw new Error("Missing summary properties");
const state = schemas.StateResource;
if (!state || !("x-studio-entity-scope" in state))
  throw new Error("Missing state entity scope");
const summaryFields = Object.keys(summary.properties ?? {});
const scope: unknown = state["x-studio-entity-scope"];
if (
  !scope ||
  typeof scope !== "object" ||
  !("collections" in scope) ||
  !Array.isArray(scope.collections)
)
  throw new Error("Missing state collections");
const globalCollections = new Set<unknown>(scope.collections);
const recorded = await readFile(input, "utf8");
if (!recorded.endsWith("\n\n"))
  throw new Error("Recording ends with an incomplete SSE event");
const frames = recorded
  .slice(0, -2)
  .split("\n\n")
  .map((frame) => `${frame}\n\n`);
const summaries = new Map<string, string>();
const transformed: string[] = [];
let skippedDetails = 0;
let suppressedSummaries = 0;
let agentRows = 0;
let agentValueBytes = 0;
let summaryValueBytes = 0;
for (const frame of frames) {
  const lines = frame.trimEnd().split("\n");
  const text = lines
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trimStart())
    .join("\n");
  if (!text || !lines.some((line) => line === "event: resources")) {
    transformed.push(frame);
    continue;
  }
  const event: components["schemas"]["ResourceChangeEvent"] = JSON.parse(text);
  const resources: components["schemas"]["ResourceRef"][] = [];
  const versions: unknown[] = [];
  for (const [index, entry] of (event.resourceVersions ?? []).entries()) {
    const resource = entry.resource ?? event.resources[index];
    if (!resource) throw new Error("Missing resource identity");
    if (resource?.kind !== "state") {
      // Background replay has no open chat or mounted panel.
      if (resource?.kind !== "drafts") {
        skippedDetails++;
        continue;
      }
      resources.push(resource);
      versions.push(entry);
      continue;
    }
    const batch = entry.entityChanges;
    if (!batch) {
      resources.push(resource);
      versions.push(entry);
      continue;
    }
    const documents: unknown[] = [];
    for (const row of batch.documents) {
      if (row.collection === "agent") {
        const mapped: Omit<typeof row, "value"> & {
          value?: Record<string, unknown>;
        } = { ...row, collection: "agentSummary" };
        if (row.op === "upsert") {
          const value: Record<string, unknown> = { ...row.value };
          mapped.value = Object.fromEntries(
            summaryFields
              .filter((field) => field in value)
              .map((field) => [field, value[field]]),
          );
          for (const field of [
            "hasQuestion",
            "hasApproval",
            "hasBackgroundWork",
          ]) {
            if (!mapped.value[field]) delete mapped.value[field];
          }
          if (row.value.commandProgress?.length)
            mapped.value.hasBackgroundWork = true;
          // The publication owner compares canonical values before assigning seq.
          agentRows++;
          agentValueBytes += Buffer.byteLength(JSON.stringify(row.value));
          summaryValueBytes += Buffer.byteLength(JSON.stringify(mapped.value));
          const canonical = JSON.stringify(mapped.value);
          if (summaries.get(row.id) === canonical) {
            suppressedSummaries++;
            continue;
          }
          summaries.set(row.id, canonical);
        } else summaries.delete(row.id);
        documents.push(mapped);
      } else if (globalCollections.has(row.collection)) documents.push(row);
      else skippedDetails++;
    }
    if (documents.length) {
      resources.push(resource);
      versions.push({ ...entry, entityChanges: { ...batch, documents } });
    }
  }
  if (!resources.length) continue;
  const replayed = { ...event, resources, resourceVersions: versions };
  transformed.push(
    `${lines.filter((line) => !line.startsWith("data:")).join("\n")}\ndata: ${JSON.stringify(replayed)}\n\n`,
  );
}
async function compress(events: string[]) {
  const encoder = createBrotliCompress({
    params: {
      [constants.BROTLI_PARAM_QUALITY]: quality,
      [constants.BROTLI_PARAM_LGWIN]: lgwin,
    },
  });
  const chunks: Buffer[] = [];
  encoder.on("data", (chunk: Buffer) => chunks.push(chunk));
  for (const frame of events) {
    encoder.write(frame);
    await new Promise<void>((resolve) =>
      encoder.flush(constants.BROTLI_OPERATION_FLUSH, resolve),
    );
  }
  const ended = new Promise((resolve, reject) => {
    encoder.once("end", resolve);
    encoder.once("error", reject);
  });
  encoder.end();
  await ended;
  const compressed = Buffer.concat(chunks);
  const raw = Buffer.from(events.join(""));
  if (!brotliDecompressSync(compressed).equals(raw))
    throw new Error("Replay round-trip failed");
  return {
    events: events.length,
    identityBytes: raw.length,
    brotliBytes: compressed.length,
  };
}
const before = await compress(frames);
const after = await compress(transformed);
const report = {
  method:
    "recorded background replay; one shared Brotli dictionary, flush per event",
  quality,
  lgwin,
  summaryFields,
  agentRows,
  agentValueBytes,
  summaryValueBytes,
  skippedDetails,
  suppressedSummaries,
  before,
  after,
  ratio: before.brotliBytes / after.brotliBytes,
};
await writeFile(output, `${JSON.stringify(report, null, 2)}\n`);
console.log(JSON.stringify(report));
