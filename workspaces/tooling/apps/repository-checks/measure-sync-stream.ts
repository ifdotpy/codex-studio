#!/usr/bin/env node
// Read-only traffic sampling. Payload recording is explicit and server state is untouched.
import http from "node:http";
import type { components } from "../../../client/apps/web/src/generated/api.ts";
import { createWriteStream } from "node:fs";
import https from "node:https";
import { createBrotliDecompress } from "node:zlib";
import { readFile, writeFile } from "node:fs/promises";
import { parseArgs } from "node:util";

// Inspect the contract owner as tooling metadata; do not import application code.
const protocolOwner = await readFile(
  new URL(
    "../../../runtime/packages/runtime-schema/src/sync_protocol.rs",
    import.meta.url,
  ),
  "utf8",
);
const protocolVersion = protocolOwner.match(
  /pub const SYNC_PROTOCOL_VERSION: u32 = (\d+);/,
)?.[1];
if (!protocolVersion)
  throw new Error("Cannot read SYNC_PROTOCOL_VERSION from its Rust owner");

const { values } = parseArgs({
  options: {
    url: { type: "string" },
    seconds: { type: "string", default: "30" },
    protocol: { type: "string", default: protocolVersion },
    resources: { type: "string" },
    "resources-file": { type: "string" },
    output: { type: "string" },
    record: { type: "string" },
    "token-rates": { type: "boolean", default: true },
  },
});
const seconds = Number(values.seconds);
if (!values.url || !Number.isFinite(seconds) || seconds <= 0)
  throw new Error(
    "Usage: --url URL --seconds N [--protocol 3] [--resources JSON | --resources-file FILE] [--output FILE]",
  );
const resources: components["schemas"]["ResourceRef"][] = JSON.parse(
  values["resources-file"]
    ? await readFile(values["resources-file"], "utf8")
    : values.resources || '[{"kind":"state"},{"kind":"drafts"}]',
);
if (!Array.isArray(resources)) throw new Error("resources must be an array");
const url = new URL("/api/sync/stream", values.url);
url.searchParams.set("protocol", values.protocol ?? protocolVersion);
url.searchParams.set("resources", JSON.stringify(resources));
url.searchParams.set("tokenRates", String(values["token-rates"]));
const byteLength = (value: unknown) => Buffer.byteLength(JSON.stringify(value));

function sample(acceptEncoding: string) {
  return new Promise((resolve, reject) => {
    const started = performance.now();
    const startedAt = new Date().toISOString();
    const report = {
      acceptEncoding,
      contentEncoding: "identity",
      startedAt,
      seconds,
      wireBodyBytes: 0,
      decodedBodyBytes: 0,
      incompleteFrameBytes: 0,
      elapsedSeconds: 0,
      events: {} as Record<
        string,
        {
          count: number;
          bytes: number;
          initialCount: number;
          initialBytes: number;
        }
      >,
      resourceKinds: {} as Record<
        string,
        { references: number; resourceAndVersionBytes: number }
      >,
      entityCollections: {} as Record<string, { count: number; bytes: number }>,
      entityChurn: {} as Record<
        string,
        {
          updates: number;
          wholeValueBytes: number;
          changedFieldsBytes: number;
          fields: Record<string, { changes: number; bytes: number }>;
        }
      >,
      resources: {} as Record<
        string,
        { references: number; resourceAndVersionBytes: number }
      >,
    };
    const previousEntities = new Map<string, Record<string, unknown>>();
    const recording =
      acceptEncoding === "identity" && values.record
        ? createWriteStream(values.record, { mode: 0o600 })
        : null;
    let pending = "";
    let response: http.IncomingMessage | undefined;
    let decoder: ReturnType<typeof createBrotliDecompress> | undefined;
    let done = false;
    const finish = (error?: Error) => {
      if (done) return;
      done = true;
      clearTimeout(deadline);
      report.incompleteFrameBytes = Buffer.byteLength(pending);
      report.elapsedSeconds = (performance.now() - started) / 1000;
      if (!response && !error)
        error = new Error(
          "stream did not send response headers before deadline",
        );
      request.destroy();
      response?.destroy();
      decoder?.destroy();
      const settle = () => (error ? reject(error) : resolve(report));
      if (recording) recording.end(settle);
      else settle();
    };
    const deadline = setTimeout(() => finish(), seconds * 1000);
    const request = (url.protocol === "https:" ? https : http).get(
      url,
      {
        headers: { "Accept-Encoding": acceptEncoding },
      },
      (incoming) => {
        response = incoming;
        if (incoming.statusCode !== 200)
          return finish(
            new Error(`stream returned HTTP ${incoming.statusCode}`),
          );
        report.contentEncoding =
          incoming.headers["content-encoding"] || "identity";
        incoming.on("data", (chunk: Buffer) => {
          report.wireBodyBytes += Buffer.byteLength(chunk);
        });
        incoming.on("error", finish);
        const body =
          report.contentEncoding === "br"
            ? incoming.pipe((decoder = createBrotliDecompress()))
            : incoming;
        if (!["br", "identity"].includes(report.contentEncoding))
          return finish(
            new Error(`Unsupported Content-Encoding ${report.contentEncoding}`),
          );
        body.setEncoding("utf8");
        body.on("error", finish);
        body.on("data", (chunk: string) => {
          report.decodedBodyBytes += Buffer.byteLength(chunk);
          recording?.write(chunk);
          pending += chunk;
          let end: number;
          while ((end = pending.indexOf("\n\n")) !== -1) {
            const frame = pending.slice(0, end + 2);
            pending = pending.slice(end + 2);
            const lines = frame.split("\n");
            const name =
              lines
                .find((line) => line.startsWith("event:"))
                ?.slice(6)
                .trim() || "message";
            const stats = (report.events[name] ||= {
              count: 0,
              bytes: 0,
              initialCount: 0,
              initialBytes: 0,
            });
            stats.count++;
            stats.bytes += Buffer.byteLength(frame);
            const data = lines
              .filter((line) => line.startsWith("data:"))
              .map((line) => line.slice(5).trimStart())
              .join("\n");
            if (!data) continue;
            const event: components["schemas"]["ResourceChangeEvent"] =
              JSON.parse(data);
            if (event.reason === "initial") {
              stats.initialCount++;
              stats.initialBytes += Buffer.byteLength(frame);
            }
            for (const [index, ref] of (event.resources || []).entries()) {
              const version = event.resourceVersions?.[index];
              const resource = version?.resource || ref;
              const kind = (report.resourceKinds[resource.kind] ||= {
                references: 0,
                resourceAndVersionBytes: 0,
              });
              const key = JSON.stringify(resource);
              const detail = (report.resources[key] ||= {
                references: 0,
                resourceAndVersionBytes: 0,
              });
              detail.references++;
              detail.resourceAndVersionBytes +=
                byteLength(ref) + (version ? byteLength(version) : 0);
              kind.references++;
              kind.resourceAndVersionBytes +=
                byteLength(ref) + (version ? byteLength(version) : 0);
              for (const row of version?.entityChanges?.documents || []) {
                const collection =
                  row.collection ||
                  String(Reflect.get(row, "id") ?? "").split(":")[1] ||
                  "unknown";
                const entity = (report.entityCollections[collection] ||= {
                  count: 0,
                  bytes: 0,
                });
                entity.count++;
                entity.bytes += byteLength(row);
                const payload: { value?: Record<string, unknown> } | null =
                  "payload" in row &&
                  typeof row.payload === "string" &&
                  row.payload
                    ? JSON.parse(row.payload)
                    : null;
                const value: Record<string, unknown> | null =
                  ("value" in row ? { ...row.value } : null) ??
                  payload?.value ??
                  null;
                const key = `${collection}:${row.id}`;
                const previous = previousEntities.get(key);
                if (value && previous) {
                  const churn = (report.entityChurn[collection] ||= {
                    updates: 0,
                    wholeValueBytes: 0,
                    changedFieldsBytes: 0,
                    fields: {},
                  });
                  churn.updates++;
                  churn.wholeValueBytes += byteLength(value);
                  const changed: Record<string, unknown> = {};
                  for (const field of new Set([
                    ...Object.keys(previous),
                    ...Object.keys(value),
                  ])) {
                    if (
                      JSON.stringify(previous[field]) ===
                      JSON.stringify(value[field])
                    )
                      continue;
                    changed[field] = value[field] ?? null;
                    const stats = (churn.fields[field] ||= {
                      changes: 0,
                      bytes: 0,
                    });
                    stats.changes++;
                    stats.bytes += byteLength({
                      [field]: value[field] ?? null,
                    });
                  }
                  churn.changedFieldsBytes += byteLength(changed);
                }
                if (value) previousEntities.set(key, value);
                else previousEntities.delete(key);
              }
            }
          }
        });
        body.on("end", () => finish());
      },
    );
    request.on("error", finish);
    recording?.on("error", reject);
  });
}

// Concurrent subscriptions see the same normal activity; this creates no load generator.
const samples = await Promise.all([sample("identity"), sample("br")]);
const report = {
  url: url.origin,
  protocol: values.protocol,
  resources,
  tokenRates: values["token-rates"],
  samples,
};
const output = JSON.stringify(report, null, 2) + "\n";
if (values.output) await writeFile(values.output, output);
process.stdout.write(output);
