#!/usr/bin/env node
// Offline replay only: no server reads or writes. Payloads stay outside the checkout.
import { readFile, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import {
  createBrotliCompress,
  brotliDecompressSync,
  constants,
} from "node:zlib";

const [input, output] = process.argv.slice(2);
if (!input || !output)
  throw new Error("usage: replay-view-scoped.mjs <recorded.sse> <report.json>");
const schema = JSON.parse(
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
const encoderValue = (name) => {
  const value = source.match(new RegExp(`const ${name}: u32 = (\\d+);`))?.[1];
  if (!value) throw new Error(`Missing stream encoder owner ${name}`);
  return Number(value);
};
const quality = encoderValue("STREAM_BROTLI_QUALITY");
const lgwin = encoderValue("STREAM_BROTLI_LGWIN");
const schemas = schema.components.schemas;
const summaryFields = Object.keys(schemas.AgentSummaryEntityDto.properties);
const globalCollections = new Set(
  schemas.StateResource["x-studio-entity-scope"].collections,
);
const recorded = await readFile(input, "utf8");
if (!recorded.endsWith("\n\n"))
  throw new Error("Recording ends with an incomplete SSE event");
const frames = recorded
  .slice(0, -2)
  .split("\n\n")
  .map((frame) => `${frame}\n\n`);
const summaries = new Map();
const transformed = [];
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
  const event = JSON.parse(text);
  const resources = [];
  const versions = [];
  for (const [index, entry] of (event.resourceVersions ?? []).entries()) {
    const resource = entry.resource ?? event.resources[index];
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
    const documents = [];
    for (const row of batch.documents) {
      if (row.collection === "agent") {
        const mapped = { ...row, collection: "agentSummary" };
        if (row.op === "upsert") {
          mapped.value = Object.fromEntries(
            summaryFields
              .filter((field) => field in row.value)
              .map((field) => [field, row.value[field]]),
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
  event.resources = resources;
  event.resourceVersions = versions;
  transformed.push(
    `${lines.filter((line) => !line.startsWith("data:")).join("\n")}\ndata: ${JSON.stringify(event)}\n\n`,
  );
}
async function compress(events) {
  const encoder = createBrotliCompress({
    params: {
      [constants.BROTLI_PARAM_QUALITY]: quality,
      [constants.BROTLI_PARAM_LGWIN]: lgwin,
    },
  });
  const chunks = [];
  encoder.on("data", (chunk) => chunks.push(chunk));
  const flush = promisify(encoder.flush.bind(encoder));
  for (const frame of events) {
    encoder.write(frame);
    await flush(constants.BROTLI_OPERATION_FLUSH);
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
