import { useEffect, useRef, useState } from "react";
import { ActionIcon, Popover } from "@mantine/core";
import { AudioLines, X } from "lucide-react";
import { api, errorText, saved, save } from "../api";
import "./realtime-voice.css";

type VoiceRecord = { id: string; seq: number; kind: string; text: string };
type Session = { session_id: string; state: string; sdp?: string; error?: string; ended?: number };
type Peer = { id: string; connection: RTCPeerConnection; stream?: MediaStream; audio: HTMLAudioElement; channel: RTCDataChannel; remoteSet: boolean; ready: boolean; closed: boolean; submitted: boolean; registered: boolean; deadline: ReturnType<typeof setTimeout> };

// A chat switch tears down this peer, including a pending microphone permission.
export default function RealtimeVoice(props: { agentId: string; notify: (text: string) => void }) {
  return <NativeVoice key={props.agentId} {...props} />;
}
function NativeVoice({ agentId, notify }: { agentId: string; notify: (text: string) => void }) {
  const [opened, setOpened] = useState(false);
  const [active, setActive] = useState(false);
  const [muted, setMuted] = useState(false);
  const [status, setStatus] = useState("");
  const [error, setError] = useState("");
  const [blockedAudio, setBlockedAudio] = useState(false);
  const [rows, setRows] = useState<VoiceRecord[]>([]);
  const [partial, setPartial] = useState("");
  const [legacy, setLegacy] = useState("");
  const peer = useRef<Peer | null>(null);
  const generation = useRef(0);
  const muteWanted = useRef(false);
  const mounted = useRef(true);
  const cursor = useRef(0);
  const post = <T = any,>(action: string, body: object = {}) => api<T>(`/api/voice/${action}`, { agent: agentId, ...body }, { timeoutMs: 15000 });
  const end = (p: Peer) => {
    p.closed = true; clearTimeout(p.deadline);
    p.channel.close(); p.connection.close();
    p.stream?.getTracks().forEach(t => t.stop());
    p.audio.pause(); p.audio.srcObject = null;
    if (p.submitted) void post("end", { session_id: p.id }).catch(e => {
      if (mounted.current && peer.current === null) setError(`Microphone stopped. ${errorText(e)}`);
    });
  };
  const close = () => {
    ++generation.current; muteWanted.current = false;
    const p = peer.current; peer.current = null;
    if (p) end(p);
    if (mounted.current) { setActive(false); setMuted(false); setPartial(""); setStatus(""); }
  };
  const failure = (e: unknown) => { close(); if (mounted.current) setError(errorText(e)); };
  const apply = async (p: Peer, state: Session) => {
    if (peer.current !== p || p.closed) return;
    if (state.error || state.ended) {
      if (state.error) failure(new Error(state.error)); else close();
      return;
    }
    if (state.sdp && !p.remoteSet) {
      p.remoteSet = true; clearTimeout(p.deadline);
      await p.connection.setRemoteDescription({ type: "answer", sdp: state.sdp });

    }
  };
  useEffect(() => {
    mounted.current = true;
    // Preserve unsaved text from the retired courier. Never automatically send it.
    const pending = saved<Record<string, any>[]>(`voice-pending:${agentId}`, []);
    const draft = saved<{ editedText?: string } | null>(`voice-delivery:${agentId}`, null);
    setLegacy(draft?.editedText || pending.map(r => r.text || "").filter(Boolean).join("\n\n"));
    return () => { mounted.current = false; close(); };
  }, []);
  useEffect(() => {
    if (!opened && !active) return;
    let disposed = false;
    let fetching = false;
    const update = async () => {
      if (fetching) return;
      fetching = true;
      try {
        const data = await post<{ records: VoiceRecord[]; cursor: number; session?: Session }>("records", { after: cursor.current });
        if (disposed) return;
        cursor.current = data.cursor;
        setRows(previous => [...new Map([...previous, ...data.records].map(r => [r.id, r])).values()]);
        const p = peer.current;
        if (p?.registered && data.session?.session_id === p.id) await apply(p, data.session);
      } catch (e) {
        // Stop capture on loss of Studio, even if the media peer still works.
        if (!disposed) { if (peer.current) failure(e); else setError(errorText(e)); }
      } finally { fetching = false; }
    };
    void update();
    const timer = window.setInterval(() => void update(), active ? 1200 : 5000);
    return () => { disposed = true; window.clearInterval(timer); };
  }, [opened, active]);
  const start = async () => {
    if (peer.current) return;
    const ticket = ++generation.current;
    setError(""); setBlockedAudio(false); setStatus("Connecting…"); setActive(true);
    let p: Peer | null = null;
    try {
      await post("status");
      if (ticket !== generation.current) return;
      if (!window.isSecureContext || !navigator.mediaDevices) throw new Error("Voice needs HTTPS or the desktop app");
      const connection = new RTCPeerConnection();
      const audio = new Audio(); audio.autoplay = true;
      const channel = connection.createDataChannel("oai-events");
      p = { id: crypto.randomUUID(), connection, audio, channel, remoteSet: false, ready: false, closed: false, submitted: false, registered: false, deadline: setTimeout(() => { if (ticket === generation.current) failure(new Error("Voice did not connect. Start again.")); }, 90000) };
      peer.current = p;
      const current = p;
      connection.ontrack = e => {
        if (current.closed) return;
        audio.srcObject = e.streams[0] || new MediaStream([e.track]);
        void audio.play().catch(() => { if (!current.closed) setBlockedAudio(true); });
      };
      connection.onconnectionstatechange = () => {
        if (current.closed) return;
        if (connection.connectionState === "failed") failure(new Error("Voice disconnected. The saved transcript remains."));
        if (connection.connectionState === "disconnected") setStatus("Reconnecting…");
        if (connection.connectionState === "connected") setStatus(current.ready ? "Listening" : "Connecting…");
      };
      channel.onmessage = message => {
        if (current.closed) return;
        let event;
        try { event = JSON.parse(message.data); } catch { return; }
        if (event.type === "error") { failure(new Error(event.error?.message || "Voice failed")); return; }
        if (event.type === "session.started") { current.ready = true; setStatus("Listening"); }
        if (event.type === "input_transcript.added") { setStatus("Listening"); setPartial(event.item?.text || ""); }
        if (event.type === "output_transcript.added") { setStatus("Speaking"); setPartial(event.item?.text || ""); }
        if (event.type === "delegation.created") { setStatus("Working"); setPartial(""); }
        if (event.type === "turn.done") { setPartial(""); setStatus("Listening"); }
        // Native Core owns delegation and transcript persistence. Do not mirror either.
      };
      if (window.codexDesktop && !window.codexDesktop.requestMicrophone)
        throw new Error("Update the desktop app to enable microphone access");
      if (window.codexDesktop?.requestMicrophone) await window.codexDesktop.requestMicrophone();
      if (current.closed || ticket !== generation.current) return;
      const stream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true } });
      if (current.closed || ticket !== generation.current) { stream.getTracks().forEach(t => t.stop()); return; }
      current.stream = stream;
      stream.getTracks().forEach(t => { t.enabled = !muteWanted.current; t.onended = () => { if (!current.closed) failure(new Error("Microphone disconnected")); }; connection.addTrack(t, stream); });
      const offer = await connection.createOffer(); await connection.setLocalDescription(offer);
      if (current.closed) return;
      current.submitted = true;
      const result = await post<Session>("start", { session_id: current.id, sdp: offer.sdp });
      current.registered = true;
      if (current.closed) { end(current); return; }
      await apply(current, result);
    } catch (e) {
      if (ticket === generation.current) { failure(e); notify(errorText(e)); }
      else if (p && !p.closed) end(p);
    }
  };
  return <Popover opened={opened} onChange={setOpened} position="top-start" width={360} withinPortal>
    <Popover.Target><ActionIcon type="button" size="lg" variant="subtle" color={active ? "blue" : "gray"}
      aria-label="Voice conversation" title="Voice conversation" aria-pressed={active} onClick={() => setOpened(!opened)}><AudioLines size={18} /></ActionIcon></Popover.Target>
    <Popover.Dropdown className="realtime-voice">
      <header className="realtime-voice-heading"><strong>Voice</strong><ActionIcon type="button" variant="subtle" color="gray" size="sm" aria-label="Close voice panel" onClick={() => setOpened(false)}><X size={16} /></ActionIcon></header>
      {!active && !error && <p className="realtime-voice-note">Talk directly to this chat’s orchestrator.</p>}
      <div className="realtime-voice-actions">
        <button type="button" onClick={() => active ? close() : void start()}>{active ? "End voice" : "Start voice"}</button>
        {active && <button type="button" onClick={() => { const next = !muted; muteWanted.current = next; peer.current?.stream?.getAudioTracks().forEach(t => { t.enabled = !next; }); setMuted(next); }}>{muted ? "Unmute" : "Mute"}</button>}
        {active && blockedAudio && <button type="button" onClick={() => void peer.current?.audio.play().then(() => setBlockedAudio(false)).catch(e => setError(errorText(e)))}>Resume audio</button>}
      </div>
      {status && <p role="status">{status}</p>}
      {error && <p role="alert">{error}</p>}
      {partial && <p className="realtime-voice-note">{partial}</p>}
      {active && <small>AI voice · ChatGPT account</small>}
      {!!rows.length && <details><summary>Transcript</summary><div className="realtime-voice-transcript">{rows.filter(r => r.text && ["user", "courier", "assistant", "orchestrator"].includes(r.kind)).map(r => <div key={r.id}><strong>{r.kind === "user" ? "You" : r.kind === "courier" ? "Previous voice assistant" : "Codex"}</strong><p>{r.text}</p></div>)}</div></details>}
      {legacy && <details><summary>Recovered voice draft</summary><p>{legacy}</p><button type="button" onClick={() => void navigator.clipboard.writeText(legacy).then(() => { save(`voice-recovered:${agentId}`, legacy); }).catch(e => setError(errorText(e)))}>Copy draft</button></details>}
    </Popover.Dropdown>
  </Popover>;
}
