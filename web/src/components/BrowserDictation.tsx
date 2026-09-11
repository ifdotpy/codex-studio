import { useEffect, useRef, useState } from "react";
import { Popover } from "@mantine/core";
import { Mic } from "lucide-react";
import "./realtime-voice.css";

// Web Speech is implemented by the browser. It is not the Electron speech bridge.
type Recognition = {
  lang: string;
  continuous: boolean;
  interimResults: boolean;
  onresult:
    | ((event: {
        results: ArrayLike<{ isFinal: boolean; 0: { transcript: string } }>;
      }) => void)
    | null;
  onerror: ((event: { error: string }) => void) | null;
  onstart: (() => void) | null;
  onend: (() => void) | null;
  start(): void;
  stop(): void;
  abort(): void;
};
type SpeechWindow = Window & {
  SpeechRecognition?: new () => Recognition;
  webkitSpeechRecognition?: new () => Recognition;
};
export function BrowserDictation({
  chatId,
  onInsert,
  disabled = false,
}: {
  chatId: string;
  onInsert: (text: string) => void;
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(false);
  const [status, setStatus] = useState("");
  const [error, setError] = useState("");
  const [text, setText] = useState("");
  const [partial, setPartial] = useState("");
  const [locale, setLocale] = useState(navigator.language);
  const recognition = useRef<Recognition | null>(null);
  const stopTimer = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  const key = `dictation-review:${chatId}`;
  const Constructor =
    (window as SpeechWindow).SpeechRecognition ||
    (window as SpeechWindow).webkitSpeechRecognition;
  const supported = window.isSecureContext && !!Constructor;
  const persist = (value: string) => {
    setText(value);
    try {
      localStorage.setItem(key, value);
    } catch {
      setError(
        "The text could not be saved on this device. Copy it before you leave.",
      );
    }
  };
  useEffect(() => {
    try {
      setText(localStorage.getItem(key) || "");
    } catch {
      setError("Saved text is unavailable on this device.");
    }
    return () => {
      clearTimeout(stopTimer.current);
      const current = recognition.current;
      recognition.current = null;
      if (current) {
        current.onresult = null;
        current.onend = null;
        current.onerror = null;
        current.onstart = null;
        current.abort();
      }
    };
  }, [key]);
  const start = () => {
    if (!supported || !Constructor || recognition.current) return;
    setError("");
    setPartial("");
    setStatus("Starting microphone…");
    setActive(true);
    const current = new Constructor();
    const prefix = text.trim();
    recognition.current = current;
    current.lang = locale;
    current.continuous = true;
    current.interimResults = true;
    current.onstart = () => {
      if (recognition.current === current) setStatus("Listening");
    };
    current.onresult = (event) => {
      if (recognition.current !== current) return;
      const final: string[] = [],
        pending: string[] = [];
      Array.from(event.results).forEach((result) =>
        (result.isFinal ? final : pending).push(result[0].transcript),
      );
      persist([prefix, final.join(" ")].filter(Boolean).join("\n"));
      setPartial(pending.join(" "));
    };
    current.onerror = (event) => {
      if (recognition.current !== current) return;
      const messages: Record<string, string> = {
        "not-allowed":
          "Microphone access was denied. Allow access in the browser settings, then try again.",
        "service-not-allowed":
          "Speech recognition is unavailable in this browser. Use keyboard dictation or the Mac app.",
        network:
          "The browser speech service lost its connection. The text already shown is saved on this device.",
        "no-speech":
          "No speech was detected. Check the microphone, then try again.",
        "audio-capture":
          "The microphone is unavailable. Check its connection and browser access.",
        "language-not-supported":
          "The browser speech service does not support this language.",
      };
      if (event.error !== "aborted")
        setError(
          messages[event.error] ||
            "Dictation stopped. The text already shown is saved on this device.",
        );
      if (event.error !== "aborted") current.abort();
    };
    current.onend = () => {
      if (recognition.current !== current) return;
      clearTimeout(stopTimer.current);
      recognition.current = null;
      setActive(false);
      setPartial("");
      setStatus("Microphone stopped. Review the text.");
    };
    try {
      current.start();
    } catch (failure) {
      recognition.current = null;
      setActive(false);
      setStatus("");
      setError(String(failure));
    }
  };
  const stop = () => {
    const current = recognition.current;
    if (!current) return;
    setStatus("Finishing text…");
    current.stop();
    stopTimer.current = setTimeout(() => {
      if (recognition.current !== current) return;
      current.abort();
      recognition.current = null;
      setActive(false);
      setPartial("");
      setStatus("Microphone stopped. Review the text.");
      setError(
        "The browser did not return the final words. Check the text before you insert it.",
      );
    }, 5000);
  };
  return (
    <>
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
            className={`dictation-trigger ${active ? "is-recording" : ""}`}
            aria-label="Dictation"
            title="Dictation"
            disabled={disabled && !active}
            onClick={() => setOpen(!open)}
          >
            <Mic size={18} />
          </button>
        </Popover.Target>
        <Popover.Dropdown className="dictation-popover">
          <header>
            <strong>Dictation</strong>
          </header>
          <p className="dictation-note">
            Your browser converts speech to text. Its speech service may receive
            audio. Studio saves only the text on this device.
          </p>
          {!supported && (
            <p role="status">
              Dictation is unavailable in this browser. Use keyboard dictation
              or the Mac app.
            </p>
          )}
          <label className="dictation-language">
            Language
            <select
              aria-label="Dictation language"
              value={locale}
              disabled={active}
              onChange={(event) => setLocale(event.target.value)}
            >
              {[
                ...new Set([
                  navigator.language,
                  "en-US",
                  "ru-RU",
                  "de-DE",
                  "fr-FR",
                  "es-ES",
                  "pl-PL",
                  "uk-UA",
                ]),
              ].map((code) => (
                <option key={code} value={code}>
                  {new Intl.DisplayNames([navigator.language], {
                    type: "language",
                  }).of(code) || code}
                </option>
              ))}
            </select>
          </label>
          {!active && (
            <button
              type="button"
              className="dictation-record"
              disabled={!supported || disabled}
              onClick={start}
            >
              Record
            </button>
          )}
          {active && open && (
            <button type="button" className="dictation-record" onClick={stop}>
              Stop dictation
            </button>
          )}
          {status && open && <p role="status">{status}</p>}
          {partial && <p className="dictation-note">{partial}</p>}
          {error && (
            <p role="alert" className="dictation-error">
              {error}
            </p>
          )}
          <label className="dictation-review">
            Review text
            <textarea
              aria-label="Review dictated text"
              value={text}
              disabled={active}
              onChange={(event) => persist(event.target.value)}
            />
          </label>
          <button
            type="button"
            className="dictation-record"
            disabled={active || !text.trim()}
            onClick={() => onInsert(text)}
          >
            Insert into message
          </button>
        </Popover.Dropdown>
      </Popover>
      {active && !open && (
        <div
          className="voice-active-bar"
          role="group"
          aria-label="Active dictation"
        >
          <span role="status">Dictation: {status}</span>
          <button type="button" onClick={stop}>
            Stop dictation
          </button>
        </div>
      )}
    </>
  );
}
