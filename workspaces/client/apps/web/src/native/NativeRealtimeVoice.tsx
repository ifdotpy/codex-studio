import { useEffect, useMemo, useRef, useState } from "react";
import { ActionIcon, Popover } from "@mantine/core";
import { AudioLines, X } from "lucide-react";
import { nativeServerContext } from "./client";
import {
  createNativeVoiceSession,
  type NativeVoiceState as VoiceState,
} from "./voiceSession";
import { useServerServices } from "../servers/ServerServicesContext";
import ErrorDescription from "../components/ErrorDescription";
import "../components/realtime-voice.css";

type VoiceRecord = { id: string; kind: string; text?: string };
type Props = { agentId: string; notify: (text: string) => void };
export default function NativeRealtimeVoice(props: Props) {
  const services = useServerServices();
  const owner = services?.owner.id ?? "local";
  return (
    <VoicePanel key={`${owner}:${props.agentId}`} {...props} owner={owner} />
  );
}
function VoicePanel({ agentId, notify, owner }: Props & { owner: string }) {
  const context = useMemo(() => nativeServerContext({ id: owner }), [owner]);
  const [opened, setOpened] = useState(false);
  const [busy, setBusy] = useState(false);
  const [value, setValue] = useState<VoiceState>({
    active: false,
    status: "",
    partial: "",
    muted: false,
  });
  const [rows, setRows] = useState<VoiceRecord[]>([]);
  const [viewInfo, setViewInfo] = useState<{
    maxRecords: number;
    maxBytes: number;
    trimmed: boolean;
    gap: boolean;
  } | null>(null);
  const [error, setError] = useState<unknown>(null);
  const live = useRef(true);
  const session = useRef("");
  useEffect(() => {
    let closed = false;
    live.current = true;
    const stops: Array<() => Promise<void>> = [];
    void context
      .then(async (scope) => {
        if (closed) return;
        const stopState = await scope.subscribe<VoiceState>(
          "voice-session",
          { agent: agentId },
          ({ value, error }) => {
            if (closed) return;
            if (error) setError(error.message);
            if (value) {
              session.current = value.sessionId ?? "";
              setValue(value);
              if (value.error) setError(value.error);
            }
          },
        );
        stops.push(stopState);
        if (closed) {
          await stopState();
          return;
        }
      })
      .catch((failure) => {
        if (!closed) setError(failure);
      });
    return () => {
      closed = true;
      live.current = false;
      for (const stop of stops) void stop().catch(() => {});
      const id = session.current;
      if (id)
        void context
          .then((scope) => createNativeVoiceSession(scope, agentId).end(id))
          .catch(() => {});
    };
  }, [context, agentId]);
  const watchRecords = opened || value.active;
  useEffect(() => {
    if (!watchRecords) return;
    let closed = false;
    let stop: (() => Promise<void>) | undefined;
    void context
      .then(async (scope) => {
        if (closed) return;
        stop = await scope.subscribe<{
          type: string;
          snapshot?: {
            resource?: { kind: string; agentId?: string };
            data?: {
              records: VoiceRecord[];
              view?: {
                maxRecords: number;
                maxBytes: number;
                trimmed: boolean;
                gap: boolean;
              };
            };
          };
        }>(
          "subscriptions",
          { resources: [{ kind: "voice", agentId }] },
          ({ value, error }) => {
            if (closed) return;
            if (error) setError(error.message);
            const snapshot = value?.snapshot;
            if (
              snapshot?.resource?.kind === "voice" &&
              snapshot.resource.agentId === agentId &&
              snapshot.data?.records
            ) {
              setRows(snapshot.data.records);
              setViewInfo(snapshot.data.view ?? null);
            }
          },
        );
        if (closed) await stop();
      })
      .catch((failure) => {
        if (!closed) setError(failure);
      });
    return () => {
      closed = true;
      void stop?.().catch(() => {});
    };
  }, [context, agentId, watchRecords]);
  const run = async (work: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await work();
    } catch (failure) {
      if (live.current) {
        setError(failure);
        notify(String(failure));
      }
    } finally {
      if (live.current) setBusy(false);
    }
  };
  const start = () =>
    run(async () => {
      const scope = await context;
      const voice = createNativeVoiceSession(scope, agentId);
      const requestId = await scope.request<string>("system.uuid", {});
      await voice.permit();
      if (!live.current) return;
      try {
        const next = await voice.start(requestId);
        if (!live.current) {
          if (next.sessionId) await voice.end(next.sessionId);
          return;
        }
        session.current = next.sessionId ?? "";
        setValue(next);
      } catch (failure) {
        // A read cannot repeat Start after its response was lost.
        const saved = await voice.read(requestId).catch(() => null);
        if (saved?.active && live.current) {
          session.current = saved.sessionId ?? "";
          setValue(saved);
        } else if (saved?.active && saved.sessionId)
          await voice.end(saved.sessionId);
        throw failure;
      }
    });
  const end = () =>
    run(async () => {
      const scope = await context;
      const next = await createNativeVoiceSession(scope, agentId).end(
        session.current,
      );
      if (live.current) setValue(next);
    });
  const mute = () =>
    run(async () => {
      const scope = await context;
      const next = await createNativeVoiceSession(scope, agentId).mute(
        session.current,
        !value.muted,
      );
      if (live.current) setValue(next);
    });
  return (
    <Popover
      opened={opened}
      onChange={setOpened}
      position="top-start"
      width={360}
      withinPortal
    >
      <Popover.Target>
        <ActionIcon
          type="button"
          size="lg"
          variant="subtle"
          color={value.active ? "blue" : "gray"}
          aria-label="Voice conversation"
          title="Voice conversation"
          aria-pressed={value.active}
          onClick={() => setOpened(!opened)}
        >
          <AudioLines size={18} />
        </ActionIcon>
      </Popover.Target>
      <Popover.Dropdown className="realtime-voice">
        <header className="realtime-voice-heading">
          <strong>Voice</strong>
          <ActionIcon
            type="button"
            variant="subtle"
            aria-label="Close voice panel"
            onClick={() => setOpened(false)}
          >
            <X size={16} />
          </ActionIcon>
        </header>
        {!value.active && !error && (
          <p className="realtime-voice-note">
            Talk directly to this chat’s orchestrator.
          </p>
        )}
        <div className="realtime-voice-actions">
          {value.active ? (
            <>
              <button
                type="button"
                disabled={busy}
                aria-pressed={value.muted}
                onClick={() => void mute()}
              >
                {value.muted ? "Unmute" : "Mute"}
              </button>
              <button type="button" disabled={busy} onClick={() => void end()}>
                End voice
              </button>
            </>
          ) : (
            <button type="button" disabled={busy} onClick={() => void start()}>
              Start voice
            </button>
          )}
        </div>
        {value.active && (
          <p role="status">{value.muted ? "Microphone muted" : value.status}</p>
        )}
        {error != null && (
          <p role="alert">
            <ErrorDescription value={error} />
          </p>
        )}
        {value.partial && (
          <p className="realtime-voice-note">{value.partial}</p>
        )}
        {value.active && (
          <small>
            Voice ends when you leave this chat. The task keeps running.
          </small>
        )}
        {!!rows.length && (
          <details>
            <summary>Recent transcript</summary>
            <small>This view can omit older records.</small>
            {viewInfo?.gap && (
              <p role="alert">
                Some transcript records are missing from this view.
              </p>
            )}
            {viewInfo?.trimmed && (
              <p>
                The view keeps at most {viewInfo.maxRecords} records and{" "}
                {viewInfo.maxBytes} bytes.
              </p>
            )}
            <div className="realtime-voice-transcript">
              {rows
                .filter(
                  (row) =>
                    row.text &&
                    ["user", "courier", "assistant", "orchestrator"].includes(
                      row.kind,
                    ),
                )
                .map((row) => (
                  <div key={row.id}>
                    <strong>{row.kind === "user" ? "You" : row.kind}</strong>
                    <p>{row.text}</p>
                  </div>
                ))}
            </div>
          </details>
        )}
      </Popover.Dropdown>
    </Popover>
  );
}
