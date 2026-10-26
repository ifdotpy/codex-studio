import { describe, expect, it, vi } from "vitest";
import type { NativeServerContext } from "./client";
import { createNativeByteImport, createNativeBytes } from "./bytes";

describe("native byte handles", () => {
  it("passes exact old upload identities and opaque files without reading browser bytes", async () => {
    const request = vi.fn().mockResolvedValue(null);
    const context = {
      request,
      serverId: "remote",
      workspaceId: "a".repeat(32),
    } as unknown as NativeServerContext;
    const bytes = createNativeBytes(context);
    const file = {
      nativeHandle: "private-handle",
      name: "old.txt",
      type: "text/plain",
      size: 8,
    };
    await bytes.queueUploads("/state", "chat", [file], context.workspaceId);
    expect(request).toHaveBeenLastCalledWith("uploads.enqueue", {
      stateDir: "/state",
      agent: "chat",
      files: [file],
      workspace: context.workspaceId,
    });
    const row = {
      id: "original-id",
      workspace: context.workspaceId,
      stateDir: "/state",
      agent: "chat",
      name: "old.txt",
      size: 8,
      mime: "text/plain",
      created: 1,
    };
    await bytes.deliverUpload(row);
    expect(request).toHaveBeenLastCalledWith(
      "uploads.deliver",
      { row },
      "original-id",
    );
  });
  it("keeps uncertain transcription as one native request without a byte or WAV fallback", async () => {
    const request = vi.fn().mockRejectedValue(new Error("response lost"));
    const context = { request } as unknown as NativeServerContext;
    const row = {
      id: "recording",
      chatId: "chat",
      created: 1,
      sampleRate: 16000,
      samples: 2,
      state: "ready" as const,
    };
    await expect(
      createNativeBytes(context).transcribeRecording(
        row,
        "permit",
        "en-US",
        "original-attempt",
      ),
    ).rejects.toThrow("response lost");
    expect(request).toHaveBeenCalledTimes(1);
    expect(request).toHaveBeenCalledWith(
      "speech.transcribe",
      {
        recording: "recording",
        chatId: "chat",
        permit: "permit",
        locale: "en-US",
        id: "original-attempt",
      },
      "original-attempt",
    );
  });
  it("readback uses the same manifest and cannot remove the source", async () => {
    const request = vi.fn().mockResolvedValue({});
    const importer = createNativeByteImport({
      request,
    } as unknown as NativeServerContext);
    await importer.commit("original-manifest");
    await importer.readback("original-manifest");
    expect(request.mock.calls).toEqual([
      ["files.importCommit", { manifestId: "original-manifest" }],
      ["files.importReadback", { manifestId: "original-manifest" }],
    ]);
  });
});
