import { useEffect, useRef, useState } from "react";
import { Popover } from "@mantine/core";
import { Download, Mic, Square, Trash2 } from "lucide-react";
import { captureAudio } from "../dictation/capture";
import {
  deleteRecording,
  listRecordings,
  recordingAudio,
  saveChunk,
  saveRecording,
  updateRecording,
  beginTranscription,
  trashRecording,
  restoreRecording,
  deletedRecordings,
  type Recording,
} from "../dictation/storage";
import "./Dictation.css";
import { BrowserDictation } from "./BrowserDictation";
const duration = (seconds: number) =>
  `${Math.floor(seconds / 60)}:${String(Math.floor(seconds % 60)).padStart(2, "0")}`;
type DictationProps = {
  chatId: string;
  onInsert: (text: string) => void;
  disabled?: boolean;
};
export function Dictation(props: DictationProps) {
  return window.codexDesktop ? (
    <NativeDictation {...props} />
  ) : (
    <BrowserDictation {...props} />
  );
}
function NativeDictation({
  chatId,
  onInsert,
  disabled = false,
}: {
  chatId: string;
  onInsert: (text: string) => void;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false),
    [rows, setRows] = useState<Recording[]>([]),
    [error, setError] = useState(""),
    [busy, setBusy] = useState("");
  const [locale, setLocale] = useState(navigator.language);
  const [deleted, setDeleted] = useState<Recording[]>([]);
  const [progress, setProgress] = useState("");
  const attemptRef = useRef("");
  const cancelledAttempt = useRef("");
  const supported =
    !!window.codexDesktop?.requestMicrophone &&
    !!window.codexDesktop?.prepareTranscription &&
    !!window.codexDesktop?.transcribeAudio;
  const [elapsed, setElapsed] = useState(0),
    [recording, setRecording] = useState(false);
  const capture = useRef<Awaited<ReturnType<typeof captureAudio>> | null>(null);
  const mounted = useRef(true),
    active = useRef<Recording | null>(null),
    queue = useRef(Promise.resolve());
  const durable = useRef<Recording | null>(null);
  const storageFailed = useRef(false);
  const scope = useRef(chatId);
  scope.current = chatId;
  const load = async () => {
    const result = await listRecordings(chatId);
    const removed = await deletedRecordings(chatId);
    if (mounted.current && scope.current === chatId) {
      setRows(result);
      setDeleted(removed);
    }
  };
  const fail = (e: unknown) => {
    if (mounted.current) setError(e instanceof Error ? e.message : String(e));
  };
  const stop = async () => {
    let ready: Recording | undefined;
    if (mounted.current) {
      setBusy("stop");
      setRecording(false);
    }
    const recorder = capture.current;
    capture.current = null;
    let finalError: unknown;
    if (recorder) {
      try {
        await recorder.stop();
      } catch (error) {
        finalError = error;
      }
    }
    try {
      await queue.current;
      const row = active.current;
      active.current = null;
      if (row) {
        const saved = durable.current || row;
        if (saved.samples) {
          ready = {
            ...saved,
            state: "ready",
            ...(finalError
              ? {
                  error:
                    finalError instanceof Error
                      ? finalError.message
                      : String(finalError),
                }
              : {}),
          };
          await updateRecording(ready);
        } else await deleteRecording(row.id);
      }
    } catch (error) {
      finalError ||= error;
    } finally {
      if (mounted.current) {
        setRecording(false);
        setBusy("");
        try {
          await load();
        } catch (error) {
          finalError ||= error;
        }
      }
    }
    if (finalError) throw finalError;
    return ready;
  };
  useEffect(() => {
    mounted.current = true;
    setRows([]);
    setError("");
    void load().catch(fail);
    return () => {
      mounted.current = false;
      void stop().catch(() => {});
    };
  }, [chatId]);
  useEffect(
    () =>
      window.codexDesktop?.onTranscriptionProgress?.((value) => {
        if (value.id === attemptRef.current)
          setProgress(`${value.completed} of ${value.total} parts`);
      }),
    [],
  );
  useEffect(() => {
    if (open) void load().catch(fail);
  }, [open]);
  useEffect(() => {
    if (!recording) return;
    const id = setInterval(() => setElapsed((n) => n + 1), 1000);
    return () => clearInterval(id);
  }, [recording]);
  const start = async () => {
    setError("");
    if (!supported) {
      setError("Update the desktop app before you record dictation.");
      return;
    }
    setBusy("start");
    const owner = chatId;
    let index = 0;
    try {
      // Call the native permission bridge synchronously within the button gesture.
      if (window.codexDesktop && !window.codexDesktop.requestMicrophone)
        throw Error(
          "Install the updated desktop app to enable microphone access.",
        );
      if (window.codexDesktop?.requestMicrophone)
        await window.codexDesktop.requestMicrophone();
      if (!mounted.current || scope.current !== owner) return;
      const row: Recording = {
        id: crypto.randomUUID(),
        chatId: owner,
        created: Date.now(),
        sampleRate: 16000,
        samples: 0,
        state: "recording",
      };
      await saveRecording(row);
      if (!mounted.current || scope.current !== owner) {
        await deleteRecording(row.id);
        return;
      }
      active.current = row;
      durable.current = { ...row };
      storageFailed.current = false;
      const recorder = await captureAudio((pcm) => {
        if (!active.current || active.current.id !== row.id) return;
        row.samples += pcm.length;
        const snapshot = { ...row },
          number = index++;
        queue.current = queue.current
          .then(async () => {
            if (storageFailed.current) return;
            await saveChunk(snapshot, number, pcm);
            durable.current = snapshot;
          })
          .catch((e) => {
            storageFailed.current = true;
            fail(
              Error(
                `Recording stopped: ${String(e)}. Saved audio remains available.`,
              ),
            );
            void stop().catch(fail);
          });
        if (row.samples / row.sampleRate >= 1800) {
          setError("The 30 minute recording limit was reached.");
          void stop().catch(fail);
        }
      });
      row.sampleRate = recorder.sampleRate;
      capture.current = recorder;
      if (!mounted.current || scope.current !== owner) {
        await stop();
        return;
      }
      setElapsed(0);
      setRecording(true);
      setBusy("");
    } catch (e) {
      await stop().catch(() => {});
      fail(e);
      if (mounted.current) setBusy("");
    }
  };
  const transcribe = async (
    row: Recording,
    reservation?: ReturnType<typeof prepare>,
  ) => {
    const owner = chatId,
      attempt = crypto.randomUUID();
    setBusy(row.id);
    setError("");
    setProgress("Preparing audio…");
    attemptRef.current = attempt;
    try {
      // Reserve the native gesture synchronously, then claim this saved recording.
      const prepared =
        reservation ||
        (window.codexDesktop?.transcribeAudio
          ? window.codexDesktop.prepareTranscription().then(
              (permit) => ({ permit, error: undefined }),
              (error) => ({ permit: undefined, error }),
            )
          : Promise.resolve({
              permit: undefined,
              error: Error(
                "Transcription requires the updated Codex Studio desktop app on macOS. Your recording is saved.",
              ),
            }));
      if (!(await beginTranscription(row, attempt)))
        throw Error("The recording was deleted.");
      const authorization = await prepared;
      if (authorization.error) throw authorization.error;
      if (cancelledAttempt.current === attempt)
        throw Error("Transcription cancelled. Your recording is saved.");
      const audio = await recordingAudio(row);
      if (cancelledAttempt.current === attempt)
        throw Error("Transcription cancelled. Your recording is saved.");
      const result = await window.codexDesktop!.transcribeAudio({
        id: attempt,
        permit: authorization.permit!,
        audio: await audio.arrayBuffer(),
        locale,
      });
      if (cancelledAttempt.current === attempt)
        throw Error("Transcription cancelled. Your recording is saved.");
      await updateRecording(
        {
          ...row,
          transcriptionAttempt: attempt,
          state: "ready",
          transcript: result.text,
          reviewedText: undefined,
          error: undefined,
        },
        attempt,
      );
    } catch (e) {
      await updateRecording(
        {
          ...row,
          transcriptionAttempt: attempt,
          state: "ready",
          error: e instanceof Error ? e.message : String(e),
        },
        attempt,
      ).catch(() => {});
    } finally {
      if (mounted.current && scope.current === owner) {
        setBusy("");
        attemptRef.current = "";
        setProgress("");
        await load().catch(fail);
      }
    }
  };
  function prepare() {
    return window.codexDesktop!.prepareTranscription().then(
      (permit) => ({ permit, error: undefined }),
      (error) => ({ permit: undefined, error }),
    );
  }
  const finish = async () => {
    // Reserve native authorization while Stop is still a trusted button gesture.
    const reservation = prepare();
    const row = await stop();
    if (row && mounted.current) await transcribe(row, reservation);
  };
  return (
    <Popover
      opened={open}
      onChange={setOpen}
      position="top-start"
      width={360}
      withinPortal
    >
      <Popover.Target>
        <button
          type="button"
          className={`dictation-trigger ${recording ? "is-recording" : ""}`}
          aria-label="Dictation"
          title="Dictation"
          disabled={disabled && !recording && !busy}
          onClick={() => setOpen(!open)}
        >
          <Mic size={18} />
          {recording && <span>{duration(elapsed)}</span>}
        </button>
      </Popover.Target>
      <Popover.Dropdown className="dictation-popover">
        <header>
          <strong>Dictation</strong>
          <span>Audio saved on this device</span>
        </header>
        <p className="dictation-note">
          Stop to transcribe on this Mac. Review the text before you insert it.
        </p>
        {!supported && (
          <p role="status">
            Update the desktop app before you record dictation.
          </p>
        )}
        <label className="dictation-language">
          Language{" "}
          <select
            aria-label="Dictation language"
            value={locale}
            disabled={!!busy || recording}
            onChange={(event) => setLocale(event.target.value)}
          >
            {Array.from(
              new Set([
                navigator.language,
                "en-US",
                "ru-RU",
                "de-DE",
                "fr-FR",
                "es-ES",
                "pl-PL",
                "uk-UA",
              ]),
            ).map((code) => (
              <option key={code} value={code}>
                {new Intl.DisplayNames([navigator.language], {
                  type: "language",
                }).of(code) || code}
              </option>
            ))}
          </select>
        </label>
        <button
          type="button"
          className="dictation-record"
          disabled={!!busy || !supported}
          onClick={() => void (recording ? finish() : start()).catch(fail)}
        >
          {recording ? <Square size={14} /> : <Mic size={14} />}{" "}
          {recording
            ? `Stop · ${duration(elapsed)}`
            : busy === "start"
              ? "Starting…"
              : "Record"}
        </button>
        {attemptRef.current && (
          <div className="dictation-progress">
            <p role="status">Transcribing… {progress}</p>
            <button
              type="button"
              onClick={() => {
                cancelledAttempt.current = attemptRef.current;
                void window.codexDesktop
                  ?.cancelTranscription?.(attemptRef.current)
                  .catch(fail);
              }}
            >
              Cancel transcription
            </button>
          </div>
        )}
        {!!deleted.length && (
          <div role="status" className="dictation-undo">
            {deleted.length} deleted. Undo is available for 24 hours.{" "}
            <button
              type="button"
              onClick={() =>
                void Promise.all(deleted.map(restoreRecording))
                  .then(load)
                  .catch(fail)
              }
            >
              Undo delete
            </button>
          </div>
        )}
        {error && (
          <p role="alert" className="dictation-error">
            {error}
          </p>
        )}
        <div className="dictation-recordings">
          {rows.map((row) => (
            <RecordingItem
              key={row.id}
              row={row}
              busy={!!busy || recording}
              transcribing={busy === row.id}
              onTranscribe={() => void transcribe(row)}
              onReview={(reviewedText) =>
                void updateRecording({ ...row, reviewedText }).catch(fail)
              }
              onDelete={() => void trashRecording(row).then(load).catch(fail)}
              onInsert={(text) => {
                if (row.transcript && row.chatId === scope.current) {
                  try {
                    onInsert(text);
                  } catch (e) {
                    fail(e);
                  }
                }
              }}
            />
          ))}
        </div>
      </Popover.Dropdown>
    </Popover>
  );
}
function RecordingItem({
  row,
  busy,
  transcribing,
  onTranscribe,
  onDelete,
  onInsert,
  onReview,
}: {
  row: Recording;
  busy: boolean;
  transcribing: boolean;
  onTranscribe: () => void;
  onDelete: () => void;
  onInsert: (text: string) => void;
  onReview: (text: string) => void;
}) {
  const [url, setUrl] = useState("");
  const [review, setReview] = useState(
    row.reviewedText ?? row.transcript ?? "",
  );
  useEffect(
    () => setReview(row.reviewedText ?? row.transcript ?? ""),
    [row.transcript, row.reviewedText],
  );
  useEffect(() => {
    let alive = true,
      objectURL = "";
    void recordingAudio(row).then((blob) => {
      objectURL = URL.createObjectURL(blob);
      if (alive) setUrl(objectURL);
      else URL.revokeObjectURL(objectURL);
    });
    return () => {
      alive = false;
      if (objectURL) URL.revokeObjectURL(objectURL);
    };
  }, [row.id, row.samples]);
  return (
    <section className="dictation-item">
      <div className="dictation-item-heading">
        <span>
          {new Date(row.created).toLocaleTimeString([], {
            hour: "2-digit",
            minute: "2-digit",
          })}{" "}
          · {duration(row.samples / row.sampleRate)}
          {row.state === "recording" ? " · Recovered" : ""}
        </span>
        <a
          href={url || undefined}
          download={`dictation-${row.id}.wav`}
          aria-label="Download recording"
        >
          <Download size={14} />
        </a>
        <button
          type="button"
          disabled={busy}
          onClick={onDelete}
          aria-label="Delete recording"
        >
          <Trash2 size={14} />
        </button>
      </div>
      {url && <audio controls src={url} preload="metadata" />}
      {row.error && (
        <p role="alert" className="dictation-error">
          {row.error}
        </p>
      )}
      {row.transcript && (
        <label className="dictation-review">
          Review text
          <textarea
            aria-label="Review dictated text"
            value={review}
            disabled={busy}
            onChange={(event) => {
              setReview(event.target.value);
              onReview(event.target.value);
            }}
          />
        </label>
      )}
      <div className="dictation-actions">
        <button
          type="button"
          disabled={busy || !row.samples}
          onClick={onTranscribe}
        >
          {transcribing
            ? "Transcribing…"
            : row.error || row.transcript
              ? "Retry transcription"
              : "Transcribe"}
        </button>
        {row.transcript && (
          <button
            type="button"
            disabled={busy || !review.trim()}
            onClick={() => onInsert(review)}
          >
            Insert into message
          </button>
        )}
      </div>
    </section>
  );
}
