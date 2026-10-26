import { useCallback, useEffect, useState } from "react";
import FilePreviewView, {
  type PreviewTarget,
} from "../components/FilePreviewView";
import StreamingText from "../components/conversation/transcript/StreamingText";
import { useServerFileActions } from "./servicesContext";
import { useNativePreview } from "./usePreview";
import type { NativeFilePreview, NativePlaybackReceipt } from "./preview";
import DocumentPreview from "./DocumentPreview";
import ErrorDescription from "../components/ErrorDescription";
export type { PreviewTarget } from "../components/FilePreviewView";

function Document({
  source,
  handle,
  title,
}: {
  source?: string;
  handle?: string;
  title: string;
}) {
  const owner = useNativePreview();
  const load = useCallback(
    (page: number) =>
      owner.then((port) =>
        handle
          ? port.pdf(handle, page)
          : port.document("html", source ?? "", page),
      ),
    [owner, handle, source],
  );
  return <DocumentPreview load={load} title={title} />;
}
function Media({ session }: { session: NativeFilePreview }) {
  const [receipt, setReceipt] = useState<NativePlaybackReceipt | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [attempted, setAttempted] = useState(false);
  useEffect(() => session.watchMedia(setReceipt), [session]);
  const run = (action: () => Promise<NativePlaybackReceipt>) => {
    setBusy(true);
    setError(null);
    void action()
      .then(setReceipt, setError)
      .finally(() => setBusy(false));
  };
  return (
    <div className="file-preview-media">
      <button
        type="button"
        disabled={attempted}
        onClick={() => {
          setAttempted(true);
          run(session.media);
        }}
      >
        Play media
      </button>
      <button
        type="button"
        disabled={busy || !attempted || receipt?.status !== "unknown"}
        onClick={() => run(session.inspectMedia)}
      >
        Check playback
      </button>
      <button
        type="button"
        disabled={
          !receipt?.playbackId ||
          receipt.status === "stopped" ||
          receipt.status === "failed"
        }
        onClick={() => {
          setBusy(true);
          setError(null);
          void session
            .stopMedia()
            .catch(setError)
            .finally(() => setBusy(false));
        }}
      >
        Stop playback
      </button>
      {receipt && (
        <p role="status">
          {receipt.status === "unknown"
            ? "Playback is not confirmed. Check its status before another action."
            : receipt.status === "preparing"
              ? "The device prepares the media preview."
              : receipt.status === "playing"
                ? "Media is open in the native preview."
                : receipt.status === "failed"
                  ? receipt.error || "Cannot play this media."
                  : "Playback stopped."}
        </p>
      )}
      {error != null && (
        <p>
          <ErrorDescription value={error} />
        </p>
      )}
    </div>
  );
}
export default function FilePreview({
  target,
  onClose,
}: {
  target: PreviewTarget | null;
  onClose(): void;
}) {
  const owner = useNativePreview();
  const actions = useServerFileActions();
  const key = JSON.stringify([target?.agent, target?.path, target?.asset]);
  const [loaded, setLoaded] = useState<{
    key: string;
    session: NativeFilePreview;
  } | null>(null);
  const [failure, setFailure] = useState<{
    key: string;
    error: unknown;
  } | null>(null);
  const session = loaded?.key === key ? loaded.session : null;
  useEffect(() => {
    let active = true;
    let opened: NativeFilePreview | undefined;
    setLoaded(null);
    setFailure(null);
    if (target)
      void owner
        .then((port) => port.open(target))
        .then(async (value) => {
          opened = value;
          if (active) setLoaded({ key, session: value });
          else await value.release();
        })
        .catch((error: unknown) => {
          setFailure({ key, error });
        });
    return () => {
      active = false;
      if (opened)
        void opened
          .release()
          .catch((error: unknown) => setFailure({ key, error }));
    };
  }, [owner, key]);
  const close = () => {
    if (!session) {
      onClose();
      return;
    }
    void session
      .release()
      .then(onClose, (error: unknown) => setFailure({ key, error }));
  };
  return (
    <FilePreviewView
      target={target}
      file={session?.file ?? null}
      error={failure?.key === key ? failure.error : null}
      onClose={close}
      onError={(error) => setFailure({ key, error })}
      actions={{
        ...actions,
        previewFile: (target) =>
          session &&
          (session.file.format === "audio" || session.file.format === "video")
            ? session.media()
            : actions.previewFile(target),
      }}
      save={() => (session ? session.save() : Promise.resolve(false))}
      markdown={(file, target) => (
        <StreamingText
          text={file.text}
          agentId={target.agent}
          basePath={target.asset ? undefined : target.path}
        />
      )}
      html={(file) => (
        <Document key={key} source={file.text} title={file.name} />
      )}
      pdf={(file) => (
        <Document key={key} handle={session?.handle} title={file.name} />
      )}
      media={() =>
        session ? <Media key={session.handle} session={session} /> : null
      }
    />
  );
}
