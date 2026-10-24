import { afterEach, expect, it, vi } from "vitest";
vi.mock("./client", () => ({ nativeRequest: vi.fn() }));
import { nativeRequest } from "./client";
import { saveNativeFile } from "./saveFile";
afterEach(() => vi.resetAllMocks());
it("sends bounded exact chunks then reports a cancelled save", async () => {
  const invoke = vi.mocked(nativeRequest);
  invoke.mockImplementation(async (method) =>
    method === "desktop.saveBegin"
      ? { transfer: "t" }
      : method === "desktop.saveFinish"
        ? false
        : null,
  );
  const bytes = Uint8Array.from({ length: 300001 }, (_, i) => i % 256);
  await expect(saveNativeFile("fixture.bin", bytes.buffer)).resolves.toBe(
    false,
  );
  const chunks = invoke.mock.calls
    .filter(([method]) => method === "desktop.saveChunk")
    .map(([, p]) => (p as { value: { offset: number; base64: string } }).value);
  expect(chunks.map((c) => c.offset)).toEqual([0, 256 * 1024]);
  expect(
    Buffer.concat(chunks.map((c) => Buffer.from(c.base64, "base64"))).equals(
      Buffer.from(bytes),
    ),
  ).toBe(true);
  expect(invoke.mock.calls.at(-1)?.[0]).toBe("desktop.saveFinish");
});
it("does not replay an uncertain save and only discards its transfer", async () => {
  const invoke = vi.mocked(nativeRequest);
  invoke.mockImplementation(async (method) => {
    if (method === "desktop.saveBegin") return { transfer: "t" };
    if (method === "desktop.saveFinish") throw new Error("lost reply");
    return null;
  });
  await expect(saveNativeFile("empty", new ArrayBuffer(0))).rejects.toThrow(
    "lost reply",
  );
  expect(invoke.mock.calls.map(([method]) => method)).toEqual([
    "desktop.saveBegin",
    "desktop.saveFinish",
    "desktop.saveCancel",
  ]);
});
