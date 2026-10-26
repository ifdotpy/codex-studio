import { useEffect, useMemo, useRef, useState } from "react";
import { Popover } from "@mantine/core";
import { Mic } from "lucide-react";
import { nativeServerContext } from "./client";
import { createNativeBytes, type NativeRecording } from "./bytes";
import { useServerServices } from "../servers/ServerServicesContext";
import ErrorDescription from "../components/ErrorDescription";
import { localTime } from "../local-time";
import "../components/Dictation.css";

export function NativeRecordingPanel({
  chatId,
  onInsert,
  disabled = false,
}: {
  chatId: string;
  onInsert: (text: string) => void;
  disabled?: boolean;
}) {
  const services = useServerServices();
  const owner = services?.owner.id ?? "local";
  return (
    <NativeRecordingBody
      key={`${owner}:${chatId}`}
      chatId={chatId}
      onInsert={onInsert}
      disabled={disabled}
    />
  );
}

function NativeRecordingBody({
  chatId,
  onInsert,
  disabled = false,
}: {
  chatId: string;
  onInsert: (text: string) => void;
  disabled?: boolean;
}) {
  const services = useServerServices();
  const owner = services?.owner.id ?? "local";
  const context = useMemo(() => nativeServerContext({ id: owner }), [owner]);
  const [opened, setOpened] = useState(false);
  const [rows, setRows] = useState<NativeRecording[]>([]);
  const [deleted, setDeleted] = useState<NativeRecording[]>([]);
  const [active, setActive] = useState("");
  const [busy, setBusy] = useState("");
  const [error, setError] = useState<unknown>(null);
  const [progress, setProgress] = useState("");
  const live = useRef(true);
  const capture = useRef("");
  const attempt = useRef("");
  const [locale, setLocale] = useState("");
  const [languages, setLanguages] = useState<
    Array<{ code: string; label: string }>
  >([]);
  useEffect(() => {
    live.current = true;
    let closed = false;
    let stop: (() => Promise<void>) | undefined;
    void context
      .then(async (scope) => {
        if (closed) return;
        const language = await createNativeBytes(scope).speechLanguages();
        if (closed) return;
        setLocale(language.locale);
        setLanguages(language.choices);
        stop = await scope.subscribe<{
          recordings: NativeRecording[];
          deleted: NativeRecording[];
        }>("recordings", { chatId }, ({ value, error }) => {
          if (closed) return;
          if (error) setError(error.message);
          if (value) {
            setRows(value.recordings);
            setDeleted(value.deleted);
            if (
              capture.current &&
              value.recordings.some(
                (row) => row.id === capture.current && row.state === "ready",
              )
            ) {
              capture.current = "";
              setActive("");
            }
          }
        });
        if (closed) await stop();
      })
      .catch((failure) => {
        if (!closed) setError(failure);
      });
    return () => {
      closed = true;
      live.current = false;
      void stop?.().catch(() => {});
      if (capture.current) {
        const id = capture.current;
        capture.current = "";
        void context
          .then((scope) => createNativeBytes(scope).stopRecording(id))
          .catch(() => {});
      }
    };
  }, [context, chatId]);
  const operation = async (
    name: string,
    run: (bytes: ReturnType<typeof createNativeBytes>) => Promise<unknown>,
  ) => {
    setBusy(name);
    setError(null);
    try {
      await run(createNativeBytes(await context));
    } catch (failure) {
      if (live.current) setError(failure);
    } finally {
      if (live.current) setBusy("");
    }
  };
  const start = () =>
    operation("start", async (bytes) => {
      const requestId = await (
        await context
      ).request<string>("system.uuid", {});
      await bytes.requestMicrophone();
      if (!live.current) return;
      let row: NativeRecording;
      try {
        row = await bytes.startRecording(chatId, requestId);
      } catch (failure) {
        // Recover the handle by readback. Never start another capture after a lost reply.
        const saved = await bytes.recordingState(requestId).catch(() => null);
        if (saved && live.current) {
          capture.current = saved.id;
          setActive(saved.id);
        } else if (saved) await bytes.stopRecording(saved.id);
        throw failure;
      }
      if (!live.current) {
        await bytes.stopRecording(row.id);
        return;
      }
      capture.current = row.id;
      setActive(row.id);
    });
  const stop = () =>
    operation("stop", async (bytes) => {
      const id = capture.current;
      if (!id) return;
      // Stop is the native gesture for transcription. Reserve before audio flush.
      const permit = bytes.prepareTranscription();
      // Keep a rejection observed even if the audio flush itself fails.
      const authorization = permit.then(
        (value) => ({ value, error: null }),
        (error) => ({ value: null, error }),
      );
      const row = await bytes.stopRecording(id);
      capture.current = "";
      setActive("");
      if (!live.current) return;
      const saved = await authorization;
      if (saved.error) throw saved.error;
      if (row.samples && saved.value)
        await transcribeSaved(row, bytes, saved.value);
    });
  const transcribeSaved = async (
    row: NativeRecording,
    bytes: ReturnType<typeof createNativeBytes>,
    permit: string,
  ) => {
    const scope = await context;
    const id = await scope.request<string>("system.uuid", {});
    attempt.current = id;
    setProgress("Transcribing saved audio…");
    try {
      await bytes.transcribeRecording(row, permit, locale, id);
    } catch (failure) {
      throw new Error(
        `The transcription result is not confirmed. Check the saved recording before you retry. ${String(failure)}`,
      );
    } finally {
      attempt.current = "";
      if (live.current) setProgress("");
    }
  };
  const transcribe = (row: NativeRecording) =>
    operation(row.id, async (bytes) =>
      transcribeSaved(row, bytes, await bytes.prepareTranscription()),
    );
  return (
    <Popover
      opened={opened}
      onChange={setOpened}
      position="top-start"
      width={420}
      withinPortal
    >
      <Popover.Target>
        <button
          type="button"
          title="Voice dictation"
          aria-label="Voice dictation"
          aria-pressed={!!active}
          disabled={disabled}
          onClick={() => setOpened(!opened)}
        >
          <Mic size={18} />
        </button>
      </Popover.Target>
      <Popover.Dropdown className="dictation-panel">
        <header>
          <strong>Dictation</strong>
        </header>
        <p>Stop to transcribe. Audio stays on this device.</p>
        <label>
          Language{" "}
          <select
            aria-label="Dictation language"
            value={locale}
            disabled={!!busy || !!active}
            onChange={(event) => setLocale(event.target.value)}
          >
            {languages.map((language) => (
              <option key={language.code} value={language.code}>
                {language.label}
              </option>
            ))}
          </select>
        </label>
        <button
          type="button"
          disabled={!!busy || disabled || !locale}
          onClick={active ? stop : start}
        >
          {active ? "Stop recording" : "Record"}
        </button>
        {active && (
          <p role="status">Recording. Audio stays in the native client.</p>
        )}
        {progress && <p role="status">{progress}</p>}
        {attempt.current && (
          <button
            type="button"
            onClick={() =>
              void context
                .then((scope) =>
                  createNativeBytes(scope).cancelTranscription(attempt.current),
                )
                .catch(setError)
            }
          >
            Cancel transcription
          </button>
        )}
        {error != null && (
          <div role="alert">
            <ErrorDescription value={error} />
          </div>
        )}
        {rows.map((row) => (
          <section key={row.id} className="dictation-recording">
            <header>
              <time>{localTime(new Date(row.created))}</time>
              <span>{Math.floor(row.samples / row.sampleRate)} seconds</span>
            </header>
            <div className="dictation-actions">
              <button
                type="button"
                disabled={!!busy || !!active || !row.samples}
                onClick={() => void transcribe(row)}
              >
                {row.transcript || row.error
                  ? "Retry transcription"
                  : "Transcribe"}
              </button>
              <button
                type="button"
                disabled={!!busy || !!active || !row.samples}
                onClick={() =>
                  void operation("play", (bytes) => bytes.playRecording(row.id))
                }
              >
                Play
              </button>
              <button
                type="button"
                onClick={() =>
                  void operation("play-stop", (bytes) => bytes.stopPlayback())
                }
              >
                Stop playback
              </button>
              <button
                type="button"
                disabled={!!busy || !row.samples}
                onClick={() =>
                  void operation("save", (bytes) => bytes.saveRecording(row.id))
                }
              >
                Download
              </button>
              <button
                type="button"
                disabled={!!busy || active === row.id}
                onClick={() =>
                  void operation("trash", (bytes) => bytes.trashRecording(row))
                }
              >
                Delete
              </button>
            </div>
            {row.error && (
              <p role="alert">
                <ErrorDescription value={row.error} />
              </p>
            )}
            {row.transcript && (
              <label className="dictation-review">
                Review text
                <textarea
                  value={row.reviewedText ?? row.transcript}
                  disabled={!!busy}
                  onChange={(event) => {
                    const text = event.target.value;
                    setRows((rows) =>
                      rows.map((current) =>
                        current.id === row.id
                          ? { ...current, reviewedText: text }
                          : current,
                      ),
                    );
                    void context
                      .then((scope) =>
                        createNativeBytes(scope).updateRecording({
                          id: row.id,
                          chatId: row.chatId,
                          reviewedText: text,
                        }),
                      )
                      .catch(setError);
                  }}
                />
              </label>
            )}
            {row.transcript && (
              <button
                type="button"
                disabled={
                  !!busy || !(row.reviewedText ?? row.transcript).trim()
                }
                onClick={() => onInsert(row.reviewedText ?? row.transcript!)}
              >
                Insert into message
              </button>
            )}
          </section>
        ))}
        {deleted.map((row) => (
          <button
            key={row.id}
            type="button"
            disabled={!!busy}
            onClick={() =>
              void operation("restore", (bytes) => bytes.restoreRecording(row))
            }
          >
            Undo delete
          </button>
        ))}
      </Popover.Dropdown>
    </Popover>
  );
}

// The native build aliases components/Dictation to this module, excluding browser capture.
export { NativeRecordingPanel as Dictation };
