import { expect, it, vi } from "vitest";
const request = vi.hoisted(() => vi.fn());
vi.mock("./client", () => ({ nativeRequest: request }));
import { saveApiDownload, saveTextDownload } from "./downloads";

it("does not repeat a completed native save or read browser bytes", async () => {
  const result = {
    nativeHandle: "exact-handle",
    name: "saved.log",
    type: "text/plain",
    size: 8,
    truncated: false,
    saved: true,
  };
  await expect(saveApiDownload(result)).resolves.toBe(true);
  expect(request).not.toHaveBeenCalled();
  await expect(saveApiDownload({ ...result, saved: false })).resolves.toBe(
    false,
  );
});
it("sends text to the native Save owner without Blob or an array-buffer conversion", async () => {
  request.mockResolvedValueOnce(true);
  await expect(saveTextDownload("log.txt", "original text")).resolves.toBe(
    true,
  );
  expect(request).toHaveBeenLastCalledWith("desktop.saveText", {
    value: { name: "log.txt", text: "original text" },
  });
});
