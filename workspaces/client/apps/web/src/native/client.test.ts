import { afterEach, describe, expect, it, vi } from "vitest";
import migrationContract from "../../../../contracts/native-migration.json";
import {
  hasNativeClient,
  nativeRequest,
  nativeServerContext,
  nativeStorageCutover,
} from "./client";

afterEach(() => {
  vi.unstubAllGlobals();
  vi.useRealTimers();
});
function mock(
  reply: (request: { id: string; method: string; params: unknown }) => unknown,
) {
  const invoke = vi.fn(async (_command, args) => reply(args.request));
  let identity = 0;
  vi.stubGlobal("__TAURI__", {
    core: {
      invoke: async (command: string, args: any) =>
        args.request.method === "system.uuid"
          ? {
              id: args.request.id,
              result: `00000000-0000-4000-8000-${String(++identity).padStart(12, "0")}`,
            }
          : invoke(command, args),
    },
  });
  return invoke;
}
describe("native request transport", () => {
  it("uses native identities without browser crypto", async () => {
    vi.stubGlobal("crypto", undefined);
    const invoke = mock(({ id }) => ({ id, result: true }));
    await expect(nativeRequest("read", {})).resolves.toBe(true);
    expect(invoke.mock.calls[0][1].request.id).toMatch(/^[a-f0-9-]{36}$/);
  });
  it("binds host migration and verification without accepting renderer inventories", async () => {
    const invoke = mock(({ id, method }) => ({
      id,
      result:
        method === "service.storage_cutover"
          ? {
              token: "proof",
              serverId: "remote",
              sourceIdentity: "a".repeat(64),
              migrationComplete: true,
              archive: null,
            }
          : method === migrationContract.verifyMethod
            ? { verified: true }
            : { complete: true },
    }));
    const owner = { id: "remote" };
    const proof = await nativeStorageCutover(owner);
    owner.id = "foreign";
    expect(proof.migrationComplete).toBe(true);
    await proof.verify();
    await proof.migrate();
    await proof.verify({
      serverId: "remote",
      sourceIdentity: "a".repeat(64),
      proofVersion: migrationContract.proofVersion,
      sourceDigest: "b".repeat(64),
      inventoryDigest: "c".repeat(64),
    });
    expect(invoke.mock.calls[1][1].request.params).toEqual({
      serverId: "remote",
      token: "proof",
    });
    expect(invoke.mock.calls[2][1].request.params).toEqual({
      serverId: "remote",
      token: "proof",
    });
    expect(invoke.mock.calls[2][1].request.method).toBe(
      migrationContract.migrateMethod,
    );
    expect(invoke.mock.calls[3][1].request.params).toEqual({
      serverId: "remote",
      token: "proof",
      proofVersion: migrationContract.proofVersion,
      sourceDigest: "b".repeat(64),
      inventoryDigest: "c".repeat(64),
    });
  });
  it("binds requests to the host context and keeps an exact lost-response identity", async () => {
    const sent: { id: string; method: string; params: any }[] = [];
    let lost = true;
    mock(({ id, method, params }) => {
      sent.push({ id, method, params });
      if (method === "system.uuid") return { id, result: "host-generated-id" };
      if (method === "service.open")
        return {
          id,
          result: {
            handle: "opaque",
            serverId: "remote",
            workspaceId: "a".repeat(32),
            origin: "https://owned.ts.net",
            credentialId: "host-credential",
          },
        };
      if (lost) {
        lost = false;
        throw new Error("response lost");
      }
      return { id, result: { saved: true } };
    });
    const owner = {
      id: "remote",
      origin: "https://untrusted.invalid",
      credentialId: "forged",
    };
    const context = await nativeServerContext(owner);
    expect(context.origin).toBe("https://owned.ts.net");
    expect(context.credentialId).toBe("host-credential");
    owner.id = "other";
    await expect(
      context.request(
        "outbox.enqueue",
        { body: { id: "message" } },
        "exact-write",
      ),
    ).rejects.toThrow("response lost");
    expect(
      await context.request(
        "outbox.enqueue",
        { body: { id: "message" } },
        "exact-write",
      ),
    ).toEqual({ saved: true });
    expect(
      sent.find((entry) => entry.method === "service.open")?.params,
    ).toEqual({ serverId: "remote" });
    const writes = sent.filter((entry) => entry.method === "service.request");
    expect(writes).toHaveLength(2);
    expect(writes[0]).toEqual(writes[1]);
    expect(writes[0]).toEqual({
      id: "exact-write",
      method: "service.request",
      params: {
        handle: "opaque",
        method: "outbox.enqueue",
        params: { body: { id: "message" } },
      },
    });
    expect(Object.isFrozen(context)).toBe(true);
  });
  it("retains the host's local origin and explicit absence of a credential", async () => {
    mock(({ id }) => ({
      id,
      result: {
        handle: "local-handle",
        serverId: "local",
        workspaceId: "a".repeat(32),
        origin: "http://127.0.0.1:8337",
        credentialId: null,
      },
    }));
    const context = await nativeServerContext({ id: "local" });
    expect(context.origin).toBe("http://127.0.0.1:8337");
    expect(context.credentialId).toBeNull();
  });
  it.each([
    { origin: "" },
    { origin: null },
    { credentialId: undefined },
    { credentialId: "" },
    { credentialId: 1 },
    { serverId: "other" },
    { workspaceId: "wrong" },
  ])("refuses malformed host owner metadata %j", async (invalid) => {
    mock(({ id }) => ({
      id,
      result: {
        handle: "opaque",
        serverId: "remote",
        workspaceId: "a".repeat(32),
        origin: "https://owned.ts.net",
        credentialId: "host-credential",
        ...invalid,
      },
    }));
    await expect(nativeServerContext({ id: "remote" })).rejects.toThrow(
      "owner",
    );
  });
  it("renders 150 messages below transport limits without delaying writes", async () => {
    vi.useFakeTimers();
    let active = 0;
    let peak = 0;
    let completed = 0;
    mock(async ({ id, method }) => {
      if (method !== "markdown.parse") return { id, result: "saved" };
      active++;
      peak = Math.max(peak, active);
      if (active > 4) throw new Error("render request limit exceeded");
      await new Promise((resolve) => setTimeout(resolve, 0));
      active--;
      completed++;
      return { id, result: [] };
    });
    const replies = Array.from({ length: 150 }, (_, index) =>
      nativeRequest("markdown.parse", {
        text: `message ${index}`,
        streaming: false,
      }),
    );
    expect(
      await nativeRequest("preferences.raw_set", { key: "test", value: "yes" }),
    ).toBe("saved");
    expect(active).toBe(4);
    expect(completed).toBe(0);
    await vi.runAllTimersAsync();
    expect(await Promise.all(replies)).toHaveLength(150);
    expect(completed).toBe(150);
    expect(peak).toBe(4);
  });
  it("does not send cancelled queued Markdown and releases slots after failure", async () => {
    const releases: (() => void)[] = [];
    const invoke = mock(async ({ id }) => {
      await new Promise<void>((resolve) => releases.push(resolve));
      return { id, error: { message: "parse failed" } };
    });
    const running = Array.from({ length: 4 }, () =>
      nativeRequest("markdown.parse", {}).catch(
        (error: Error) => error.message,
      ),
    );
    const controller = new AbortController();
    const queued = nativeRequest(
      "markdown.parse",
      {},
      undefined,
      controller.signal,
    );
    controller.abort();
    await expect(queued).rejects.toMatchObject({ name: "AbortError" });
    expect(invoke).toHaveBeenCalledTimes(4);
    releases.forEach((release) => release());
    expect(await Promise.all(running)).toEqual(Array(4).fill("parse failed"));
    mock(({ id }) => ({ id, result: [] }));
    expect(await nativeRequest("markdown.parse", {})).toEqual([]);
  });
  it("preserves a successful null result", async () => {
    const invoke = mock(({ id }) => ({ id, result: null }));
    expect(hasNativeClient()).toBe(true);
    expect(await nativeRequest("void", {})).toBeNull();
    expect(invoke).toHaveBeenCalledTimes(1);
  });
  it("never retries an uncertain transport failure", async () => {
    const invoke = mock(() => {
      throw new Error("connection lost");
    });
    await expect(nativeRequest("write", {})).rejects.toThrow("connection lost");
    expect(invoke).toHaveBeenCalledTimes(1);
  });
  it("rejects a mismatched identity and an ambiguous result", async () => {
    mock(() => ({ id: "wrong", result: true }));
    await expect(nativeRequest("read", null)).rejects.toThrow("mismatched");
    mock(({ id }) => ({ id, result: true, error: { message: "denied" } }));
    await expect(nativeRequest("read", null)).rejects.toThrow(
      "invalid response",
    );
  });
  it("preserves a supplied write identity across an explicit retry", async () => {
    let first = true;
    const invoke = mock(({ id }) => {
      if (first) {
        first = false;
        throw new Error("response lost");
      }
      return { id, result: { imported: 1 } };
    });
    const params = { version: 1, entries: { draft: "Ж🦀" } };
    await expect(
      nativeRequest("preferences.import", params, "migration-1"),
    ).rejects.toThrow("response lost");
    expect(invoke).toHaveBeenCalledTimes(1);
    await expect(
      nativeRequest("preferences.import", params, "migration-1"),
    ).resolves.toEqual({ imported: 1 });
    expect(invoke.mock.calls[0]).toEqual(invoke.mock.calls[1]);
  });
  it("surfaces native command errors", async () => {
    mock(({ id }) => ({
      id,
      error: {
        message: "Permission denied",
        code: "authority",
        status: 403,
        details: { outcome: "unknown" },
      },
    }));
    await expect(nativeRequest("write", null)).rejects.toThrow(
      "Permission denied",
    );
    await expect(nativeRequest("write", null)).rejects.toMatchObject({
      code: "authority",
      status: 403,
      details: { outcome: "unknown" },
    });
  });
});
