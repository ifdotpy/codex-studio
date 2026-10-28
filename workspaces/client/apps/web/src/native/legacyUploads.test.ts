import { expect, it } from "vitest";
import { importLegacyUploads, type LegacyUpload } from "./legacyUploads";

const workspace = "a".repeat(32);
const base: LegacyUpload = {
  id: "legacy-1",
  workspace,
  stateDir: "/state",
  agent: "lead",
  name: "a.txt",
  mime: "text/plain",
  size: 5,
  created: 7,
};
// Test doubles receive untrusted JSON-like params; a record check narrows them.
function record(value: unknown): Record<string, unknown> {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : {};
}

function fake(
  replies: Record<
    string,
    (params: Record<string, unknown>, id?: string) => unknown
  >,
) {
  const calls: {
    method: string;
    params: Record<string, unknown>;
    id?: string | undefined;
  }[] = [];
  const request = (async (method: string, params: unknown, id?: string) => {
    calls.push({ method, params: record(params), id });
    const reply = replies[method];
    return reply ? reply(record(params), id) : null;
  }) as Parameters<typeof importLegacyUploads>[0];
  return { calls, request };
}

it("imports each pending file once under a stable identity and skips rows the core already has", async () => {
  const core = fake({
    "uploads.get": (params) =>
      params.id === "done-1" ? { id: "done-1" } : null,
    "uploads.status": () => ({ cancelled: false, settled: false }),
    "files.begin": () => ({ nativeHandle: "h1" }),
    "files.append": () => null,
    "uploads.import": () => ({ id: "legacy-1" }),
  });
  const source = {
    list: async () => [base, { ...base, id: "done-1", name: "done.txt" }],
    bytes: async () => new Blob(["hello"]),
  };
  await importLegacyUploads(core.request, source);
  expect(
    core.calls.filter(
      (call) => call.params.id === "done-1" && call.method !== "uploads.get",
    ),
  ).toEqual([]);
  expect(core.calls.find((call) => call.method === "files.begin")).toEqual({
    method: "files.begin",
    params: { name: "a.txt", mime: "text/plain", size: 5 },
    id: "browser-upload:legacy-1:begin",
  });
  expect(
    core.calls.find((call) => call.method === "uploads.import")?.params,
  ).toEqual({
    id: "legacy-1",
    workspace,
    stateDir: "/state",
    agent: "lead",
    nativeHandle: "h1",
    created: 7,
  });
});

it("keeps a legacy asset with its upload", async () => {
  const core = fake({
    "uploads.status": () => ({ cancelled: false, settled: false }),
    "files.begin": () => ({ nativeHandle: "h1" }),
    "files.append": () => null,
    "uploads.import": () => null,
  });
  await importLegacyUploads(core.request, {
    list: async () => [
      {
        ...base,
        asset: {
          id: "legacy-1",
          name: "a.txt",
          mime: "text/plain",
          size: 5,
          image: false,
        },
      },
    ],
    bytes: async () => new Blob(["hello"]),
  });
  expect(
    core.calls.find((call) => call.method === "uploads.import")?.params.asset,
  ).toEqual({
    id: "legacy-1",
    name: "a.txt",
    mime: "text/plain",
    size: 5,
    image: false,
  });
});

it("reports a row it cannot read by name and does not drop it", async () => {
  const core = fake({
    "uploads.status": () => ({ cancelled: false, settled: false }),
  });
  await expect(
    importLegacyUploads(core.request, {
      list: async () => [{ ...base, name: "bad.txt" }],
      bytes: async () => {
        throw new Error("The saved file bytes are unavailable.");
      },
    }),
  ).rejects.toThrow(/bad\.txt.*unavailable/);
  expect(core.calls.some((call) => call.method === "uploads.import")).toBe(
    false,
  );
});
