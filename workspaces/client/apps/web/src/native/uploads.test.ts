import { expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { createNativeUploads } from "./uploads";
import { UPLOAD_PART_BYTES } from "./uploadParts";
import type { PendingUpload } from "../sync/uploads";

const workspace = "a".repeat(32);
const MiB = 1024 * 1024;
type Call = {
  method: string;
  params: Record<string, unknown>;
  id?: string | undefined;
};

// Fake core: records each command, and can lose the first reply of chosen methods.
// Test doubles receive untrusted JSON-like params; a record check narrows them.
function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : {};
}

function fakeCore(
  replies: Record<string, (params: Record<string, unknown>) => unknown>,
  lost: string[] = [],
) {
  const calls: Call[] = [];
  const request = (async (method: string, params: unknown, id?: string) => {
    calls.push({
      method,
      params: record(params),
      id: id ?? `auto-${calls.length}`,
    });
    if (lost.includes(method)) {
      lost.splice(lost.indexOf(method), 1);
      throw new Error("The native client did not answer.");
    }
    const reply = replies[method];
    if (!reply) throw new Error(`no reply for ${method}`);
    return reply(record(params));
  }) as Parameters<typeof createNativeUploads>[0];
  return { calls, request };
}
const row = (over: Partial<PendingUpload> = {}): PendingUpload => ({
  id: "up-1",
  workspace,
  stateDir: "/state",
  agent: "lead",
  name: "a.txt",
  size: 5,
  mime: "text/plain",
  created: 1,
  ...over,
});
const noLegacy = async () => {};

it("stages a large file in 1 MiB parts and queues it by handle", async () => {
  const file = new File([new Uint8Array(3 * MiB + 5)], "big.bin", {
    type: "application/octet-stream",
  });
  const core = fakeCore({
    "files.begin": () => ({
      nativeHandle: "h1",
      name: "big.bin",
      size: file.size,
    }),
    "files.append": () => null,
    "uploads.enqueue": () => [row({ id: "up-9", name: "big.bin" })],
  });
  const uploads = createNativeUploads(core.request, noLegacy);
  const rows = await uploads.queueUploads("/state", "lead", [file], workspace);
  expect(rows[0]?.id).toBe("up-9");
  const parts = core.calls.filter((call) => call.method === "files.append");
  expect(parts.map((call) => call.params.offset)).toEqual([
    0,
    MiB,
    2 * MiB,
    3 * MiB,
  ]);
  expect(parts.map((call) => atob(String(call.params.base64)).length)).toEqual([
    MiB,
    MiB,
    MiB,
    5,
  ]);
  expect(
    core.calls.find((call) => call.method === "uploads.enqueue")?.params,
  ).toEqual({
    workspace,
    stateDir: "/state",
    agent: "lead",
    files: [{ nativeHandle: "h1" }],
  });
});

it("retries a lost reply with the same request identity", async () => {
  const file = new File(["hello"], "a.txt", { type: "text/plain" });
  const core = fakeCore(
    {
      "files.begin": () => ({ nativeHandle: "h1" }),
      "files.append": () => null,
      "uploads.enqueue": () => [row()],
    },
    ["files.begin", "uploads.enqueue"],
  );
  await createNativeUploads(core.request, noLegacy).queueUploads(
    "/state",
    "lead",
    [file],
    workspace,
  );
  const begins = core.calls.filter((call) => call.method === "files.begin");
  const enqueues = core.calls.filter(
    (call) => call.method === "uploads.enqueue",
  );
  expect(begins).toHaveLength(2);
  expect(begins[0]?.id).toBe(begins[1]?.id);
  expect(enqueues).toHaveLength(2);
  expect(enqueues[0]?.id).toBe(enqueues[1]?.id);
});

it("a cancel that lands while delivery runs wins over the late reply", async () => {
  let finish!: (value: unknown) => void;
  const core = fakeCore({
    "uploads.cancel": () => null,
    "uploads.deliver": () =>
      new Promise((resolve) => {
        finish = resolve;
      }),
  });
  const uploads = createNativeUploads(core.request, noLegacy);
  const delivery = uploads.deliverUpload(row());
  await uploads.removeUpload("up-1");
  finish({ id: "up-1", name: "a.txt" });
  expect(await delivery).toBeNull();
  expect(uploads.uploadCancelled("up-1")).toBe(true);
  expect(await uploads.deliverUpload(row())).toBeNull();
  expect(
    core.calls.filter((call) => call.method === "uploads.deliver"),
  ).toHaveLength(1);
});

it("runs the legacy import once per page and keeps its error visible", async () => {
  let runs = 0;
  const uploads = createNativeUploads(fakeCore({}).request, async () => {
    runs += 1;
    if (runs === 1) throw new Error("Saved file a.txt could not be read.");
  });
  await expect(uploads.pendingUploads("/state", workspace)).rejects.toThrow(
    "a.txt could not be read",
  );
  const core = fakeCore({ "uploads.pending": () => [row()] });
  const next = createNativeUploads(core.request, async () => {
    runs += 1;
  });
  expect(await next.pendingUploads("/state", workspace)).toHaveLength(1);
  await next.pendingUploads("/state", workspace);
  expect(runs).toBe(2);
});

it("the part size equals the core owner", () => {
  const source = readFileSync(
    new URL("../../../../packages/client-core/src/uploads.rs", import.meta.url),
    "utf8",
  );
  const owner = /pub const PART_LIMIT: usize = ([0-9 *]+);/.exec(source);
  expect(owner).not.toBeNull();
  const bytes = owner![1]!
    .split("*")
    .reduce((product, factor) => product * Number(factor), 1);
  expect(UPLOAD_PART_BYTES).toBe(bytes);
});

it("a failed staging discards the parts it already sent", async () => {
  const file = new File([new Uint8Array(2 * MiB)], "big.bin", {
    type: "application/octet-stream",
  });
  const core = fakeCore({
    "files.begin": () => ({ nativeHandle: "h1" }),
    "files.append": (params) => {
      if (params.offset === MiB) throw new Error("Storage is full.");
      return null;
    },
    "files.discard": () => null,
  });
  await expect(
    createNativeUploads(core.request, noLegacy).queueUploads(
      "/state",
      "lead",
      [file],
      workspace,
    ),
  ).rejects.toThrow("Storage is full.");
  expect(
    core.calls.find((call) => call.method === "files.discard")?.params,
  ).toEqual({
    nativeHandle: "h1",
  });
  expect(core.calls.some((call) => call.method === "uploads.enqueue")).toBe(
    false,
  );
});
