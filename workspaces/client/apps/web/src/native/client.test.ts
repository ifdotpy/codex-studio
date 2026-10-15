import { afterEach, describe, expect, it, vi } from "vitest";
import { hasNativeClient, nativeRequest } from "./client";

afterEach(() => vi.unstubAllGlobals());
function mock(
  reply: (request: { id: string; method: string; params: unknown }) => unknown,
) {
  const invoke = vi.fn(async (_command, args) => reply(args.request));
  vi.stubGlobal("__TAURI__", { core: { invoke } });
  return invoke;
}
describe("native request transport", () => {
  it("renders 150 messages below transport limits without delaying writes", async () => {
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
    expect(completed).toBeLessThan(150);
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
    mock(({ id }) => ({ id, error: { message: "Permission denied" } }));
    await expect(nativeRequest("write", null)).rejects.toThrow(
      "Permission denied",
    );
  });
});
