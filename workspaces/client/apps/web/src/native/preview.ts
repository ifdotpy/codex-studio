import type { NativeServerContext } from "./client";
import type {
  FilePresentation,
  PreviewTarget,
} from "../components/FilePreviewView";
import { randomUUID } from "./identities";

export type NativePage = {
  url: string;
  width: number;
  height: number;
  page: number;
  pages: number;
};
import {
  NATIVE_PLAYBACK_STATUSES,
  type PlaybackReceipt,
} from "../generated/rustOwned";
import { NativeCommandError } from "./commandError";
export type NativePlaybackReceipt = PlaybackReceipt;
export type NativeFilePreview = {
  handle: string;
  file: FilePresentation;
  save(): Promise<boolean>;
  media(): Promise<NativePlaybackReceipt>;
  inspectMedia(): Promise<NativePlaybackReceipt>;
  watchMedia(accept: (receipt: NativePlaybackReceipt) => void): () => void;
  stopMedia(): Promise<unknown>;
  release(): Promise<unknown>;
};
export type NativePreviewCleanup = Readonly<{
  releasePreview(handle: string): Promise<unknown>;
  inspectPreviewPlayback(requestId: string): Promise<unknown>;
  stopPreviewPlayback(token: string): Promise<unknown>;
}>;

function page(value: NativePage): NativePage {
  if (
    !value ||
    !/^data:image\/png;base64,[A-Za-z0-9+/=]+$/.test(value.url) ||
    ![value.width, value.height, value.pages].every(
      (n) => Number.isSafeInteger(n) && n > 0,
    ) ||
    !Number.isSafeInteger(value.page) ||
    value.page < 0 ||
    value.page >= value.pages
  )
    throw new Error("The native preview page is invalid.");
  return value;
}
function presentation(value: FilePresentation): FilePresentation {
  if (
    !value ||
    typeof value.name !== "string" ||
    typeof value.mime !== "string" ||
    typeof value.text !== "string" ||
    typeof value.format !== "string" ||
    typeof value.url !== "string"
  )
    throw new Error("The native file preview is invalid.");
  if (
    value.format === "image" || value.format === "svg"
      ? !/^data:image\/(?:png|jpeg|gif|webp|avif|bmp|x-icon|svg\+xml);base64,[A-Za-z0-9+/=]+$/.test(
          value.url,
        )
      : value.url !== ""
  )
    throw new Error("The native file preview contains an invalid resource.");
  if (
    value.table !== null &&
    (!value.table ||
      !Array.isArray(value.table.rows) ||
      !value.table.rows.every(
        (row) =>
          Array.isArray(row) && row.every((cell) => typeof cell === "string"),
      ) ||
      typeof value.table.error !== "string" ||
      typeof value.table.truncated !== "boolean")
  )
    throw new Error("The native file table is invalid.");
  if (
    value.json !== null &&
    (!value.json ||
      typeof value.json.text !== "string" ||
      typeof value.json.error !== "string")
  )
    throw new Error("The native file source is invalid.");
  return value;
}

/** The captured host context retains bytes, media, save permission and cleanup. */
export function createNativePreview(
  context: NativeServerContext,
  cleanup: NativePreviewCleanup,
  identity = randomUUID,
) {
  const active = new Set<NativeFilePreview>();
  const pending = new Set<Promise<NativeFilePreview>>();
  const failures: unknown[] = [];
  let disposed = false;
  let disposal: Promise<void> | undefined;
  async function prepare(
    target: PreviewTarget,
    external?: string,
  ): Promise<NativeFilePreview> {
    const targetIdentity = {
      ...(target.agent === undefined ? {} : { agent: target.agent }),
      ...(target.path === undefined ? {} : { path: target.path }),
      ...(target.asset === undefined ? {} : { asset: target.asset }),
    };
    let opened: { handle: string; length: number };
    try {
      opened = await context.request<{ handle: string; length: number }>(
        external ? "preview.externalImage" : "preview.open",
        external ? { url: external } : { target: targetIdentity },
      );
    } catch (error) {
      if (!(error instanceof NativeCommandError)) failures.push(error);
      throw error;
    }
    if (
      !opened ||
      typeof opened.handle !== "string" ||
      !opened.handle ||
      !Number.isSafeInteger(opened.length) ||
      opened.length < 0
    ) {
      const error = new Error("The native file preview receipt is invalid.");
      failures.push(error);
      throw error;
    }
    const handle = opened.handle;
    let released: Promise<unknown> | undefined;
    let releasing: Promise<unknown> | undefined;
    let requestId: string | undefined;
    let playbackId: string | undefined;
    let latest: NativePlaybackReceipt | undefined;
    let started: Promise<NativePlaybackReceipt> | undefined;
    let stopping: Promise<unknown> | undefined;
    let watching: Promise<void> | undefined;
    let unwatching: Promise<void> | undefined;
    let stopWatch: (() => Promise<void>) | undefined;
    let closed = false;
    let cleanupEffects = false;
    let unknownClose = false;
    let session: NativeFilePreview | undefined;
    const listeners = new Set<(receipt: NativePlaybackReceipt) => void>();
    const validate = (value: unknown): NativePlaybackReceipt => {
      const receipt = value as Partial<NativePlaybackReceipt> | null;
      if (
        !receipt ||
        receipt.requestId !== requestId ||
        !NATIVE_PLAYBACK_STATUSES.includes(
          receipt.status as NativePlaybackReceipt["status"],
        ) ||
        (receipt.status !== "unknown" && receipt.previewHandle !== handle) ||
        (receipt.status === "unknown" && receipt.playbackId !== undefined) ||
        (receipt.previewHandle !== undefined &&
          receipt.previewHandle !== handle) ||
        ((receipt.status === "preparing" || receipt.status === "playing") &&
          (typeof receipt.playbackId !== "string" || !receipt.playbackId)) ||
        (receipt.playbackId !== undefined &&
          (typeof receipt.playbackId !== "string" ||
            !receipt.playbackId ||
            (playbackId !== undefined && receipt.playbackId !== playbackId))) ||
        (receipt.error !== undefined && typeof receipt.error !== "string") ||
        (receipt.duration !== undefined &&
          (typeof receipt.duration !== "number" ||
            !Number.isFinite(receipt.duration) ||
            receipt.duration < 0))
      )
        throw new Error(
          "The native media receipt does not match this preview request.",
        );
      return receipt as NativePlaybackReceipt;
    };
    const publish = (receipt: NativePlaybackReceipt) => {
      // The immutable acknowledgement can arrive after a progress event.
      if (
        latest &&
        (latest.status === "stopped" ||
          latest.status === "failed" ||
          (latest.status === "playing" && receipt.status === "preparing"))
      )
        return latest;
      if (receipt.playbackId) playbackId = receipt.playbackId;
      latest = receipt;
      if (unknownClose && !cleanupEffects && receipt.status !== "unknown") {
        unknownClose = false;
        released = undefined;
      }
      for (const accept of listeners) accept(receipt);
      return receipt;
    };
    const accept = (value: unknown) => publish(validate(value));
    const uncertain = (error: unknown) => {
      if (!requestId) return;
      publish({
        requestId,
        previewHandle: handle,
        status: "unknown",
        ...(playbackId ? { playbackId } : {}),
        error: error instanceof Error ? error.message : String(error),
      });
    };
    const watch = () => {
      if (watching || !requestId) return;
      const original = requestId;
      watching = context
        .subscribe<NativePlaybackReceipt>(
          "media-preview",
          { requestId: original },
          (event) => {
            if (event.error) {
              uncertain(new Error(event.error.message));
              return;
            }
            if (event.value) {
              try {
                accept(event.value);
              } catch (error) {
                uncertain(error);
              }
            }
          },
        )
        .then((stop) => {
          stopWatch = stop;
          if (cleanupEffects) return (unwatching ??= stop());
        })
        .catch((error: unknown) => {
          uncertain(error);
          throw error;
        });
      void watching.catch(() => {});
    };
    const stopMedia = () =>
      playbackId
        ? (stopping ??= cleanup.stopPreviewPlayback(playbackId))
        : Promise.resolve();
    const release = () => {
      closed = true;
      return (released ??= (async () => {
        if (
          requestId &&
          !playbackId &&
          latest?.status !== "failed" &&
          latest?.status !== "stopped"
        ) {
          accept(await cleanup.inspectPreviewPlayback(requestId));
          if (
            !playbackId &&
            latest?.status !== "failed" &&
            latest?.status !== "stopped"
          ) {
            unknownClose = true;
            throw new Error(
              "The original media result is not confirmed. Its request was not repeated.",
            );
          }
        }
        cleanupEffects = true;
        const errors: unknown[] = [];
        try {
          await stopMedia();
        } catch (error) {
          errors.push(error);
        }
        try {
          await watching;
          if (stopWatch) await (unwatching ??= stopWatch());
        } catch (error) {
          errors.push(error);
        }
        try {
          await (releasing ??= cleanup.releasePreview(handle));
        } catch (error) {
          errors.push(error);
        }
        if (errors.length)
          throw new AggregateError(
            errors,
            "The native preview cleanup is not confirmed.",
          );
      })().then(
        () => {
          if (session) active.delete(session);
        },
        (error) => {
          if (cleanupEffects) failures.push(error);
          throw error;
        },
      ));
    };
    try {
      let offset = 0,
        text = "";
      for (;;) {
        const chunk = await context.request<{
          text: string;
          next: number;
          done: boolean;
        }>("preview.read", { handle, offset });
        if (
          !chunk ||
          typeof chunk.text !== "string" ||
          typeof chunk.done !== "boolean" ||
          !Number.isSafeInteger(chunk.next) ||
          chunk.next > opened.length ||
          (chunk.done
            ? chunk.next !== opened.length || chunk.next < offset
            : chunk.next <= offset)
        )
          throw new Error("The native file preview chunk is invalid.");
        text += chunk.text;
        offset = chunk.next;
        if (chunk.done) break;
      }
      session = {
        handle,
        file: presentation(JSON.parse(text)),
        release,
        save: () =>
          context.request<boolean>("desktop.savePreview", {
            value: { handle },
          }),
        media: async () => {
          if (closed) throw new Error("The native preview is closed.");
          if (!started) {
            requestId = identity();
            started = context
              .request<NativePlaybackReceipt>(
                "desktop.previewMedia",
                { value: { handle } },
                requestId,
              )
              .then(accept, (error: unknown) => {
                if (
                  error instanceof NativeCommandError &&
                  error.details &&
                  typeof error.details === "object" &&
                  "playback" in error.details
                ) {
                  const rejected = validate(error.details.playback);
                  if (rejected.status === "failed" && !rejected.playbackId)
                    publish(rejected);
                } else uncertain(error);
                throw error;
              });
            // Send the native action first. The snapshot on subscription covers a fast completion or a lost reply.
            watch();
          }
          return started;
        },
        inspectMedia: async () => {
          if (!requestId) throw new Error("No media request has started.");
          return accept(
            await context.request("media.playbackState", { requestId }),
          );
        },
        watchMedia(accept) {
          listeners.add(accept);
          if (latest) accept(latest);
          return () => {
            listeners.delete(accept);
          };
        },
        stopMedia,
      };
      return session;
    } catch (error) {
      try {
        await release();
      } catch (cleanup) {
        throw new AggregateError(
          [error, cleanup],
          "The native preview cleanup is not confirmed.",
        );
      }
      throw error;
    }
  }
  async function open(target: PreviewTarget, external?: string) {
    if (disposed) throw new Error("The native preview service is closed.");
    const work = prepare(target, external);
    pending.add(work);
    try {
      const session = await work;
      if (disposed) {
        await session.release();
        throw new Error("The native preview service is closed.");
      }
      active.add(session);
      return session;
    } finally {
      pending.delete(work);
    }
  }
  return {
    open,
    externalImage(url: string) {
      const parsed = new URL(url);
      if (
        !/^https?:$/.test(parsed.protocol) ||
        parsed.username ||
        parsed.password
      )
        return Promise.reject(
          new Error("The external image address is invalid."),
        );
      return open({}, parsed.toString());
    },
    pdf: (handle: string, index: number) =>
      context
        .request<NativePage>("preview.pdfPage", { handle, page: index })
        .then(page),
    async document(kind: "html" | "mermaid", source: string, index = 0) {
      let svg: string | undefined;
      if (kind === "mermaid") {
        const result = await context.request<{ svg: string }>(
          "document.diagram",
          { source },
        );
        if (typeof result?.svg !== "string")
          throw new Error("The native diagram result is invalid.");
        svg = result.svg;
      }
      return {
        ...page(
          await context.request<NativePage>("document.render", {
            source: svg ?? source,
            page: index,
          }),
        ),
        svg,
      };
    },
    copy: (text: string) =>
      context.request("desktop.copyText", { value: text }),
    saveText: (name: string, text: string) =>
      context.request<boolean>("desktop.saveText", { value: { name, text } }),
    close() {
      if (disposal) return disposal;
      disposed = true;
      return (disposal = (async () => {
        while (pending.size) await Promise.allSettled(pending);
        const results = await Promise.allSettled(
          [...active].map((session) => session.release()),
        );
        const rejected = results
          .filter(
            (result): result is PromiseRejectedResult =>
              result.status === "rejected",
          )
          .map((result) => result.reason);
        if (rejected.length || failures.length)
          throw new AggregateError(
            [...failures, ...rejected],
            "The native file previews did not close.",
          );
      })());
    },
  };
}
