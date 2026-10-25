import {
  useServerApi,
  useServerResources,
} from "../servers/ServerServicesContext";
import { hasNativeClient } from "../native/client";
import { useServerAudio } from "../servers/ServerServicesContext";
import type { nativeVoice } from "../native/voice";
import { useEffect, useRef, useState } from "react";
import { ActionIcon, Popover } from "@mantine/core";
import { AudioLines, X } from "lucide-react";
import { errorText, type PostResult } from "../api";
import type { ResourceConnectionState } from "../sync/client";
import type { ResourceVersion } from "../sync/subscriptions/events";
import "./realtime-voice.css";
import { useServerResourceReads } from "../servers/ServerServicesContext";
import { watchVoiceTransport } from "./voiceTransport";

type VoiceRecords = PostResult<"/api/voice/records">;
type VoiceRecord = VoiceRecords["records"][number];
type Session = PostResult<"/api/voice/start">;
type Peer = {
  id: string;
  connection?: RTCPeerConnection;
  native?: ReturnType<typeof nativeVoice>;
  connectionState?: string;
  stream?: MediaStream;
  audio?: HTMLAudioElement;
  channel?: RTCDataChannel;
  remoteSet: boolean;
  ready: boolean;
  closed: boolean;
  submitted: boolean;
  registered: boolean;
  deadline: ReturnType<typeof setTimeout>;
};

// A chat switch tears down this peer, including a pending microphone permission.
export default function RealtimeVoice(props: {
  agentId: string;
  notify: (text: string) => void;
}) {
  return <NativeVoice key={props.agentId} {...props} />;
}
function NativeVoice({
  agentId,
  notify,
}: {
  agentId: string;
  notify: (text: string) => void;
}) {
  const { dictationBridge, nativeVoice } = useServerAudio();
  const watchResourceReads = useServerResourceReads();
  const { post } = useServerApi();
  const { watchResourceConnection } = useServerResources();

  const [opened, setOpened] = useState(false);
  const [active, setActive] = useState(false);
  const [muted, setMuted] = useState(false);
  const [status, setStatus] = useState("");
  const [error, setError] = useState("");
  const [blockedAudio, setBlockedAudio] = useState(false);
  const [rows, setRows] = useState<VoiceRecord[]>([]);
  const [partial, setPartial] = useState("");
  const peer = useRef<Peer | null>(null);
  const generation = useRef(0);
  const muteWanted = useRef(false);
  const mounted = useRef(true);
  const starting = useRef(false);
  const transportState = useRef<ResourceConnectionState>("connecting");
  const pendingSession = useRef<Session | null>(null);
  const cursor = useRef(0);
  const cursorEpoch = useRef<string | undefined>(undefined);
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | undefined>(
    undefined,
  );
  const endSession = (session_id: string) =>
    post(
      "/api/voice/end",
      { agent: agentId, session_id },
      { timeoutMs: 15000 },
    );
  const watchVoice = opened || active;
  const end = (p: Peer) => {
    p.closed = true;
    clearTimeout(p.deadline);
    p.channel?.close();
    p.connection?.close();
    void p.native?.close();
    p.stream?.getTracks().forEach((t) => t.stop());
    p.audio?.pause();
    if (p.audio) p.audio.srcObject = null;
    if (p.submitted)
      void endSession(p.id).catch((e) => {
        if (mounted.current && peer.current === null)
          setError(
            (previous) => previous || `Microphone stopped. ${errorText(e)}`,
          );
      });
  };
  const close = () => {
    clearTimeout(reconnectTimer.current);
    ++generation.current;
    starting.current = false;
    muteWanted.current = false;
    const p = peer.current;
    peer.current = null;
    pendingSession.current = null;
    if (p) end(p);
    if (mounted.current) {
      setActive(false);
      setMuted(false);
      setPartial("");
      setStatus("");
    }
  };
  const failure = (e: unknown) => {
    close();
    if (mounted.current) setError(errorText(e));
  };
  const apply = async (p: Peer, state: Session) => {
    if (peer.current !== p || p.closed) return;
    if (state.error || state.ended) {
      if (state.error) failure(new Error(state.error));
      else close();
      return;
    }
    if (state.sdp && !p.remoteSet) {
      p.remoteSet = true;
      if (p.native) await p.native.answer(state.sdp);
      else
        await p.connection!.setRemoteDescription({
          type: "answer",
          sdp: state.sdp,
        });
    }
  };
  useEffect(() => {
    mounted.current = true;
    return () => {
      const hadVoice = !!peer.current;
      mounted.current = false;
      close();
      if (hadVoice) notify("Voice ended because you left the chat.");
    };
  }, []);
  useEffect(() => {
    if (!watchVoice) return;
    let disposed = false;
    let fetching = false;
    const stopConnection = watchVoiceTransport(
      (state) => {
        transportState.current = state;
      },
      () => {
        if (starting.current || (peer.current && !peer.current.closed)) {
          failure(
            new Error(
              "Studio connection lost. The microphone is off. Start voice again after the connection returns.",
            ),
          );
          setOpened(true);
        }
      },
      watchResourceConnection,
    );
    const update = async (version?: ResourceVersion) => {
      if (fetching) return;
      fetching = true;
      try {
        const epochChanged = !!version && cursorEpoch.current !== version.epoch;
        const after = epochChanged ? 0 : cursor.current;
        const batch = version?.voiceRecords;
        // Coalescing, a late listener, or an oversized frame can leave a gap.
        // Only those recovery paths need the read-like POST.
        const data =
          batch && batch.after <= after
            ? batch
            : await post(
                "/api/voice/records",
                { agent: agentId, after },
                { timeoutMs: 15000 },
              );
        if (disposed) return;
        cursor.current = epochChanged
          ? data.cursor
          : Math.max(cursor.current, data.cursor);
        if (version) cursorEpoch.current = version.epoch;
        setRows((previous) => [
          ...new Map(
            [...(epochChanged ? [] : previous), ...data.records].map((r) => [
              r.id,
              r,
            ]),
          ).values(),
        ]);
        const p = peer.current;
        if (p && data.session?.session_id === p.id) {
          if (p.registered) await apply(p, data.session);
          else pendingSession.current = data.session;
        }
      } catch (e) {
        // Stop capture on loss of Studio, even if the media peer still works.
        if (!disposed) {
          if (peer.current) {
            failure(
              new Error(
                `Studio connection lost. The microphone is off. Start voice again after the connection returns. ${errorText(e)}`,
              ),
            );
            setOpened(true);
          } else setError((previous) => previous || errorText(e));
        }
      } finally {
        fetching = false;
      }
    };
    const stop = watchResourceReads(
      { kind: "voice", agentId },
      update,
      (error) => {
        if (disposed) return;
        if (peer.current) {
          failure(
            new Error(
              `Studio connection lost. The microphone is off. Start voice again after the connection returns. ${errorText(error)}`,
            ),
          );
          setOpened(true);
        } else setError((previous) => previous || errorText(error));
      },
      { waitForBaseline: true },
    );
    return () => {
      disposed = true;
      stopConnection();
      stop();
    };
  }, [agentId, watchVoice, watchResourceReads, watchResourceConnection]);
  const connectionChanged = (current: Peer, state: string) => {
    current.connectionState = state;
    if (current.closed) return;
    if (state === "failed")
      failure(new Error("Voice disconnected. The saved transcript remains."));
    if (state === "disconnected") {
      setStatus("Reconnecting. Microphone paused…");
      void current.native?.mute(true).catch(failure);
      current.stream?.getAudioTracks().forEach((t) => {
        t.enabled = false;
      });
      clearTimeout(reconnectTimer.current);
      reconnectTimer.current = setTimeout(() => {
        if (peer.current === current && !current.closed) {
          failure(
            new Error(
              "Voice could not reconnect. The microphone is off. Start voice again.",
            ),
          );
          setOpened(true);
        }
      }, 12000);
    }
    if (state === "connected") {
      clearTimeout(reconnectTimer.current);
      void current.native?.mute(muteWanted.current).catch(failure);
      current.stream?.getAudioTracks().forEach((t) => {
        t.enabled = !muteWanted.current;
      });
      setStatus(current.ready ? "Listening" : "Connecting…");
    }
  };
  const messageReceived = (current: Peer, data: string) => {
    if (current.closed) return;
    let event;
    try {
      event = JSON.parse(data);
    } catch {
      return;
    }
    if (event.type === "error") {
      failure(new Error(event.error?.message || "Voice failed"));
      return;
    }
    if (event.type === "session.started") {
      clearTimeout(current.deadline);
      current.ready = true;
      setStatus("Listening");
    }
    if (event.type === "input_transcript.added") {
      setStatus("Listening");
      setPartial(event.item?.text || "");
    }
    if (event.type === "output_transcript.added") {
      setStatus("Speaking");
      setPartial(event.item?.text || "");
    }
    if (event.type === "delegation.created") {
      setStatus("Working");
      setPartial("");
    }
    if (event.type === "turn.done") {
      setPartial("");
      setStatus("Listening");
    }
    // Native Core owns delegation and transcript persistence. Do not mirror either.
  };
  const start = async () => {
    if (peer.current) return;
    const ticket = ++generation.current;
    starting.current = true;
    setError("");
    setBlockedAudio(false);
    setStatus("Connecting…");
    setActive(true);
    let p: Peer | null = null;
    try {
      if (transportState.current !== "live")
        throw new Error(
          "Studio connection is unavailable. Try voice again when connected.",
        );
      // Capture authorization while Start is still the user's native event.
      const permission = hasNativeClient()
        ? dictationBridge()!
            .requestMicrophone()
            .then(
              () => ({ error: undefined }),
              (error) => ({ error }),
            )
        : undefined;
      await post("/api/voice/status", { agent: agentId }, { timeoutMs: 15000 });
      if (ticket !== generation.current) return;
      p = {
        id: crypto.randomUUID(),
        remoteSet: false,
        ready: false,
        closed: false,
        submitted: false,
        registered: false,
        deadline: setTimeout(() => {
          if (ticket === generation.current)
            failure(new Error("Voice did not connect. Start again."));
        }, 90000),
      };
      peer.current = p;
      const current = p;
      let sdp: string;
      if (permission) {
        const authorization = await permission;
        if (authorization.error) throw authorization.error;
        if (current.closed || ticket !== generation.current) return;
        current.native = nativeVoice(current.id, (event) => {
          if (current.closed) return;
          if (event.type === "message" && event.data)
            messageReceived(current, event.data);
          else if (event.type === "state" && event.state)
            connectionChanged(current, event.state);
          else if (
            event.type === "error" ||
            (event.type === "microphone" && event.state === "ended") ||
            (event.type === "channel" && event.state === "closed")
          )
            failure(
              new Error(
                event.message ||
                  "Voice disconnected. The saved transcript remains.",
              ),
            );
        });
        sdp = await current.native.offer();
        if (current.closed || ticket !== generation.current) {
          void current.native.close();
          return;
        }
        if (muteWanted.current) await current.native.mute(true);
      } else {
        if (!window.isSecureContext || !navigator.mediaDevices)
          throw new Error("Voice needs HTTPS or the desktop app");
        const connection = new RTCPeerConnection();
        const audio = new Audio();
        audio.autoplay = true;
        const channel = connection.createDataChannel("oai-events");
        current.connection = connection;
        current.audio = audio;
        current.channel = channel;
        connection.ontrack = (e) => {
          if (current.closed) return;
          audio.srcObject = e.streams[0] || new MediaStream([e.track]);
          void audio.play().catch(() => {
            if (!current.closed) setBlockedAudio(true);
          });
        };
        connection.onconnectionstatechange = () =>
          connectionChanged(current, connection.connectionState);
        channel.onmessage = (message) => messageReceived(current, message.data);
        if (window.codexDesktop && !window.codexDesktop.requestMicrophone)
          throw new Error("Update the desktop app to enable microphone access");
        if (window.codexDesktop?.requestMicrophone)
          await window.codexDesktop.requestMicrophone();
        if (current.closed || ticket !== generation.current) return;
        const stream = await navigator.mediaDevices.getUserMedia({
          audio: { echoCancellation: true, noiseSuppression: true },
        });
        if (current.closed || ticket !== generation.current) {
          stream.getTracks().forEach((t) => t.stop());
          return;
        }
        current.stream = stream;
        stream.getTracks().forEach((t) => {
          t.enabled = !muteWanted.current;
          t.onended = () => {
            if (!current.closed) failure(new Error("Microphone disconnected"));
          };
          connection.addTrack(t, stream);
        });
        const offer = await connection.createOffer();
        await connection.setLocalDescription(offer);
        if (current.closed) return;
        sdp = offer.sdp || "";
        if (!sdp) throw new Error("Voice offer has no session description.");
      }
      if (!sdp) throw new Error("Voice offer has no session description.");
      current.submitted = true;
      const result = await post(
        "/api/voice/start",
        {
          agent: agentId,
          session_id: current.id,
          sdp,
        },
        { timeoutMs: 15000 },
      );
      current.registered = true;
      if (current.closed) {
        end(current);
        return;
      }
      const notifiedSession = pendingSession.current;
      pendingSession.current = null;
      await apply(
        current,
        notifiedSession?.session_id === current.id ? notifiedSession : result,
      );
    } catch (e) {
      if (ticket === generation.current) {
        failure(e);
        notify(errorText(e));
      } else if (p && !p.closed) end(p);
    } finally {
      starting.current = false;
    }
  };
  const toggleMute = () => {
    const next = !muted;
    muteWanted.current = next;
    peer.current?.stream?.getAudioTracks().forEach((t) => {
      t.enabled = !next && peer.current?.connectionState !== "disconnected";
    });
    void peer.current?.native
      ?.mute(next || peer.current.connectionState === "disconnected")
      .catch(failure);
    setMuted(next);
  };
  return (
    <>
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
            color={active ? "blue" : "gray"}
            aria-label="Voice conversation"
            title="Voice conversation"
            aria-pressed={active}
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
              color="gray"
              size="sm"
              aria-label="Close voice panel"
              onClick={() => setOpened(false)}
            >
              <X size={16} />
            </ActionIcon>
          </header>
          {!active && !error && (
            <p className="realtime-voice-note">
              Talk directly to this chat’s orchestrator.
            </p>
          )}
          <div className="realtime-voice-actions">
            {!active && (
              <button type="button" onClick={() => void start()}>
                Start voice
              </button>
            )}
            {active && opened && (
              <>
                <button type="button" aria-pressed={muted} onClick={toggleMute}>
                  {muted ? "Unmute" : "Mute"}
                </button>
                <button type="button" onClick={close}>
                  End voice
                </button>
              </>
            )}
            {active && blockedAudio && (
              <button
                type="button"
                onClick={() =>
                  void peer.current?.audio
                    ?.play()
                    .then(() => setBlockedAudio(false))
                    .catch((e) => setError(errorText(e)))
                }
              >
                Resume audio
              </button>
            )}
          </div>
          {active && opened && (
            <p role="status">{muted ? "Microphone muted" : status}</p>
          )}
          {error && <p role="alert">{error}</p>}
          {partial && <p className="realtime-voice-note">{partial}</p>}
          {active && (
            <small>
              Voice ends when you leave this chat. The task keeps running.
            </small>
          )}
          {!!rows.length && (
            <details>
              <summary>Transcript</summary>
              <div className="realtime-voice-transcript">
                {rows
                  .filter(
                    (r) =>
                      r.text &&
                      ["user", "courier", "assistant", "orchestrator"].includes(
                        r.kind,
                      ),
                  )
                  .map((r) => (
                    <div key={r.id}>
                      <strong>
                        {r.kind === "user"
                          ? "You"
                          : r.kind === "courier"
                            ? "Previous voice assistant"
                            : "Codex"}
                      </strong>
                      <p>{r.text}</p>
                    </div>
                  ))}
              </div>
            </details>
          )}
        </Popover.Dropdown>
      </Popover>
      {active && !opened && (
        <div
          className="voice-active-bar"
          role="group"
          aria-label="Active voice conversation"
        >
          <span role="status">{muted ? "Microphone muted" : status}</span>
          <button type="button" aria-pressed={muted} onClick={toggleMute}>
            {muted ? "Unmute" : "Mute"}
          </button>
          <button type="button" onClick={close}>
            End voice
          </button>
        </div>
      )}
    </>
  );
}
