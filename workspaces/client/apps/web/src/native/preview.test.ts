import { expect, it, vi } from "vitest";
import { createNativePreview, type NativePlaybackReceipt } from "./preview";
import type { NativeServerContext, NativeSubscriptionEvent } from "./client";
import { NativeCommandError } from "./commandError";

function receipt(
  status: NativePlaybackReceipt["status"] = "preparing",
  playbackId = "player-one",
): NativePlaybackReceipt {
  return {
    requestId: "original-media-request",
    previewHandle: "preview-one",
    status,
    playbackId,
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
function fixture(format = "text", url = "") {
  const file = {
    name: "file.txt",
    mime: "text/plain",
    format,
    text: "File source",
    url,
    table: null,
    json: null,
  };
  const text = JSON.stringify(file);
  let closed = false;
  const native = vi.fn(
    async (
      method: string,
      _params: unknown,
      _id?: string,
    ): Promise<unknown> => {
      if (method === "preview.open" || method === "preview.externalImage")
        return { handle: "preview-one", length: text.length };
      if (method === "preview.read")
        return { text, next: text.length, done: true };
      if (method === "desktop.previewMedia" || method === "media.playbackState")
        return receipt();
      return true;
    },
  );
  let receive:
    | ((event: NativeSubscriptionEvent<NativePlaybackReceipt>) => void)
    | undefined;
  const stopWatch = vi.fn(async () => {});
  const context = {
    handle: "original-context",
    serverId: "server-one",
    workspaceId: "workspace-one",
    origin: "https://server.example",
    credentialId: "host-credential",
    request: vi.fn((method: string, params: unknown, id?: string) => {
      if (closed) return Promise.reject(new Error("The service is closed."));
      return native(method, params, id);
    }),
    subscribe: vi.fn(
      async (
        _topic: string,
        _params: Record<string, unknown>,
        accept: (event: NativeSubscriptionEvent<NativePlaybackReceipt>) => void,
      ) => {
        receive = accept;
        return stopWatch;
      },
    ),
  } as unknown as NativeServerContext;
  const cleanup = {
    releasePreview: vi.fn(async (_handle: string) => {}),
    inspectPreviewPlayback: vi.fn(async (_request: string): Promise<unknown> =>
      receipt(),
    ),
    stopPreviewPlayback: vi.fn(async (_token: string) => {}),
  };
  const port = createNativePreview(
    context,
    cleanup,
    () => "original-media-request",
  );
  return {
    port,
    cleanup,
    native,
    context,
    file,
    stopWatch,
    emit: (value: NativePlaybackReceipt) => receive?.({ value }),
    closeContext: () => {
      closed = true;
    },
  };
}

it("keeps file bytes in the captured native preview and saves only its exact handle", async () => {
  const h = fixture();
  const session = await h.port.open({
    agent: "agent",
    path: "/project/file.txt",
    line: 19,
  });
  expect(h.native).toHaveBeenCalledWith(
    "preview.open",
    { target: { agent: "agent", path: "/project/file.txt" } },
    undefined,
  );
  expect(session.file).toEqual(h.file);
  await session.save();
  expect(h.native).toHaveBeenCalledWith(
    "desktop.savePreview",
    { value: { handle: "preview-one" } },
    undefined,
  );
  await session.release();
  await session.release();
  expect(h.cleanup.releasePreview).toHaveBeenCalledExactlyOnceWith(
    "preview-one",
  );
  expect(h.cleanup.stopPreviewPlayback).not.toHaveBeenCalled();
});

it("never repeats a lost media open and inspects only its original native request ID", async () => {
  const h = fixture("audio");
  const session = await h.port.open({ asset: "asset" });
  h.native.mockImplementation(async (method) => {
    if (method === "desktop.previewMedia")
      throw new Error("The reply is lost.");
    return receipt("playing", "exact-generation");
  });
  await expect(session.media()).rejects.toThrow("reply is lost");
  await expect(session.media()).rejects.toThrow("reply is lost");
  expect(
    h.native.mock.calls.filter(([method]) => method === "desktop.previewMedia"),
  ).toEqual([
    [
      "desktop.previewMedia",
      { value: { handle: "preview-one" } },
      "original-media-request",
    ],
  ]);
  await session.inspectMedia();
  expect(h.native).toHaveBeenLastCalledWith(
    "media.playbackState",
    { requestId: "original-media-request" },
    undefined,
  );
  await session.stopMedia();
  await session.release();
  expect(h.cleanup.stopPreviewPlayback).toHaveBeenCalledExactlyOnceWith(
    "exact-generation",
  );
});

it("closes a late opened handle through private cleanup after new effects are closed", async () => {
  const h = fixture();
  const opening = deferred<unknown>();
  h.native.mockImplementationOnce(() => opening.promise);
  const result = h.port
    .open({ asset: "asset" })
    .catch((error: unknown) => error);
  const close = h.port.close();
  h.closeContext();
  opening.resolve({ handle: "late-original-handle", length: 12 });
  await close;
  expect(await result).toBeInstanceOf(Error);
  expect(h.cleanup.releasePreview).toHaveBeenCalledExactlyOnceWith(
    "late-original-handle",
  );
  expect(h.cleanup.stopPreviewPlayback).not.toHaveBeenCalled();
  await expect(h.port.open({ asset: "another" })).rejects.toThrow("closed");
});

it("stops the reserved token through private cleanup before the start reply arrives", async () => {
  const h = fixture("video");
  const session = await h.port.open({ asset: "asset" });
  const media = deferred<NativePlaybackReceipt>();
  h.native.mockImplementationOnce(() => media.promise);
  h.cleanup.inspectPreviewPlayback.mockResolvedValueOnce(
    receipt("preparing", "late-original-token"),
  );
  const playing = session.media();
  const close = h.port.close();
  h.closeContext();
  await close;
  expect(h.cleanup.inspectPreviewPlayback).toHaveBeenCalledExactlyOnceWith(
    "original-media-request",
  );
  expect(h.cleanup.stopPreviewPlayback).toHaveBeenCalledExactlyOnceWith(
    "late-original-token",
  );
  expect(h.cleanup.releasePreview).toHaveBeenCalledExactlyOnceWith(
    "preview-one",
  );
  expect(h.stopWatch).toHaveBeenCalledOnce();
  media.resolve(receipt("preparing", "late-original-token"));
  await playing;
  const failed = fixture();
  await failed.port.open({ asset: "asset" });
  failed.cleanup.releasePreview.mockRejectedValueOnce(
    new Error("The release reply is lost."),
  );
  await expect(failed.port.close()).rejects.toThrow("did not close");
});

it("keeps progress when the immutable preparing acknowledgement arrives late", async () => {
  const h = fixture("video");
  const session = await h.port.open({ asset: "asset" });
  const ack = deferred<NativePlaybackReceipt>();
  h.native.mockImplementationOnce(() => ack.promise);
  const values: NativePlaybackReceipt[] = [];
  session.watchMedia((value) => values.push(value));
  const start = session.media();
  h.emit(receipt("preparing"));
  await session.stopMedia();
  h.emit(receipt("stopped"));
  ack.resolve(receipt("preparing"));
  expect(await start).toEqual(receipt("stopped"));
  expect(values.at(-1)?.status).toBe("stopped");
  expect(h.context.subscribe).toHaveBeenCalledExactlyOnceWith(
    "media-preview",
    { requestId: "original-media-request" },
    expect.any(Function),
  );
  expect(h.cleanup.stopPreviewPlayback).toHaveBeenCalledExactlyOnceWith(
    "player-one",
  );
  await session.release();
});

it("keeps an unknown close visible and cleans only a later matching token", async () => {
  const h = fixture("audio");
  const session = await h.port.open({ asset: "asset" });
  h.native.mockRejectedValueOnce(new Error("The reply is lost."));
  await expect(session.media()).rejects.toThrow("reply is lost");
  h.cleanup.inspectPreviewPlayback.mockResolvedValueOnce({
    requestId: "original-media-request",
    status: "unknown",
  });
  await expect(session.release()).rejects.toThrow("not confirmed");
  expect(h.cleanup.releasePreview).not.toHaveBeenCalled();
  expect(h.cleanup.stopPreviewPlayback).not.toHaveBeenCalled();
  h.emit(receipt("preparing", "accepted-original-token"));
  await session.release();
  expect(h.cleanup.stopPreviewPlayback).toHaveBeenCalledExactlyOnceWith(
    "accepted-original-token",
  );
  expect(h.cleanup.releasePreview).toHaveBeenCalledOnce();
  expect(
    h.native.mock.calls.filter(([method]) => method === "desktop.previewMedia"),
  ).toHaveLength(1);
});

it("rejects a foreign preview receipt and preserves the known original token", async () => {
  const h = fixture("audio");
  const session = await h.port.open({ asset: "asset" });
  await session.media();
  h.emit({
    ...receipt("playing", "foreign-token"),
    previewHandle: "another-preview",
  });
  await session.release();
  expect(h.cleanup.stopPreviewPlayback).toHaveBeenCalledExactlyOnceWith(
    "player-one",
  );
});

it("accepts an exact native rejection without claiming an unknown player exists", async () => {
  const h = fixture("video");
  const session = await h.port.open({ asset: "asset" });
  h.native.mockRejectedValueOnce(
    new NativeCommandError({
      message: "The action is unavailable.",
      details: {
        playback: {
          requestId: "original-media-request",
          previewHandle: "preview-one",
          status: "failed",
          error: "The action is unavailable.",
        },
      },
    }),
  );
  await expect(session.media()).rejects.toThrow("unavailable");
  await session.release();
  expect(h.cleanup.inspectPreviewPlayback).not.toHaveBeenCalled();
  expect(h.cleanup.stopPreviewPlayback).not.toHaveBeenCalled();
  expect(h.cleanup.releasePreview).toHaveBeenCalledOnce();
});

it("refuses renderer media bytes and credential URLs before any image request", async () => {
  for (const format of ["audio", "video"]) {
    const h = fixture(format, `data:${format}/test;base64,AQ==`);
    await expect(h.port.open({ asset: "asset" })).rejects.toThrow(
      "invalid resource",
    );
    expect(h.cleanup.releasePreview).toHaveBeenCalledOnce();
  }
  const h = fixture();
  await expect(
    h.port.externalImage("https://private:secret@example.test/image.png"),
  ).rejects.toThrow("address is invalid");
  expect(h.native).not.toHaveBeenCalled();
});

it("refuses a stalled chunk and releases the original cache without a read loop", async () => {
  const h = fixture();
  h.native.mockImplementation(async (method) =>
    method === "preview.open"
      ? { handle: "preview-one", length: 12 }
      : { text: "", next: 0, done: false },
  );
  await expect(h.port.open({ asset: "asset" })).rejects.toThrow(
    "chunk is invalid",
  );
  expect(h.native).toHaveBeenCalledTimes(2);
  expect(h.cleanup.releasePreview).toHaveBeenCalledExactlyOnceWith(
    "preview-one",
  );
});
