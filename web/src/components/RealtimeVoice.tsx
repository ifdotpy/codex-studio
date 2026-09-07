import { useEffect, useRef, useState } from "react";
import { api, errorText, saved, save } from "../api";
import "./realtime-voice.css";

type RecordRow = { id: string; seq: number; kind: string; text: string; item_id: string; previous_item_id: string; session: string; payload?: string };
type Records = { records: RecordRow[]; cursor: number; delivered: string[] };
function orderedVoiceRecords(records: RecordRow[]) {
  const sessions = new Map<string, number>();
  for (const row of records) sessions.set(row.session, Math.min(sessions.get(row.session) ?? row.seq, row.seq));
  const position = (row: RecordRow) => { try { return JSON.parse(row.payload || "{}").utterance_order ?? row.seq; } catch { return row.seq; } };
  return [...records].sort((a, b) => sessions.get(a.session)! - sessions.get(b.session)! || position(a) - position(b) || a.seq - b.seq);
}
export default function RealtimeVoice({ agentId, notify }: { agentId: string; notify: (text: string) => void }) {
  const editIds = useRef<string[] | null>(null);
  const [editedText, setEditedText] = useState<string | null>(null);
  const [active, setActive] = useState(false);
  const [busy, setBusy] = useState(false);
  const [muted, setMuted] = useState(false);
  const [status, setStatus] = useState("");
  const [rows, setRows] = useState<RecordRow[]>([]);
  const [approvals, setApprovals] = useState<{ id: string; method: string; params: unknown }[]>([]);
  const lastPermission = useRef("");
  const recorder = useRef<MediaRecorder | null>(null);
  const [sent, setSent] = useState<string[]>([]);
  const pc = useRef<RTCPeerConnection | null>(null);
  const stream = useRef<MediaStream | null>(null);
  const channel = useRef<RTCDataChannel | null>(null);
  const remote = useRef<HTMLAudioElement | null>(null);
  const speech = useRef<HTMLAudioElement | null>(null);
  const session = useRef("");
  const cursor = useRef(0);
  const live = useRef(false);
  const speaking = useRef(false);
  const responding = useRef(false);
  const pending = useRef(new Set<string>());
  const order = useRef(new Map<string, number>());
  const previous = useRef(new Map<string, string>());
  const serial = useRef(Promise.resolve());
  const queue = useRef<RecordRow[]>([]);
  const all = useRef<RecordRow[]>([]);
  const delivered = useRef(new Set<string>());
  const sendWanted = useRef(false);
  const voiceSend = useRef(false);
  const generation = useRef(0);
  const delivery = useRef<{ id: string; ids: string[]; editedText: string | null } | null>(saved(`voice-delivery:${agentId}`, null));
  const pendingRecords = useRef<Record<string, unknown>[]>(saved(`voice-pending:${agentId}`, []));
  const post = (action: string, body: object = {}) => api<any>(`/api/voice/${action}`, { agent: agentId, ...body });
  const event = (kind: string, text = "", payload: object = {}, id: string = crypto.randomUUID(), item_id = "", previous_item_id = "") => {
    const sid = session.current;
    const body = { session_id: sid, event_id: id, kind, text, payload, item_id, previous_item_id };
    pendingRecords.current.push(body); save(`voice-pending:${agentId}`, pendingRecords.current);
    const operation = serial.current.catch(() => {}).then(async () => {
      const row = await post("record", body);
      pendingRecords.current = pendingRecords.current.filter(r => r.event_id !== id); save(`voice-pending:${agentId}`, pendingRecords.current);
      if (["user", "courier"].includes(kind) && !all.current.some(r => r.id === row.id)) {
        all.current.push(row); setRows([...all.current]);
      }
    });
    serial.current = operation.catch(e => { setStatus(`Transcript not saved: ${errorText(e)}`); throw e; });
    void serial.current.catch(() => {});
    return operation;
  };
  const send = async () => {
    if (pending.current.size || speaking.current || responding.current) { sendWanted.current = true; setStatus("Waiting for the full transcript…"); return; }
    sendWanted.current = false;
    setBusy(true);
    try {
      await serial.current;
      if (pendingRecords.current.length) throw new Error("Some transcript events are not saved. Retry sync before sending.");
      if (voiceSend.current) {
        const lastUser = orderedVoiceRecords(all.current).filter(r => r.kind === "user").at(-1);
        if (!lastUser || !/^(отправляй|отправь|отправить|отправь сообщение|отправь оркестратору|send|send it|send message)[.!? ]*$/i.test(lastUser.text.trim())) throw new Error("Say отправляй as a separate command, or tap Send transcript.");
      }
      voiceSend.current = false;
      const selected = orderedVoiceRecords(all.current).filter(r => ["user", "courier"].includes(r.kind) && !delivered.current.has(r.id) && (!editIds.current || editIds.current.includes(r.id)));
      if (!delivery.current) delivery.current = { id: crypto.randomUUID(), ids: selected.map(r => r.id), editedText };
      save(`voice-delivery:${agentId}`, delivery.current);
      if (!delivery.current.ids.length) { delivery.current = null; return; }
      await post("submit", { message_id: delivery.current.id, record_ids: delivery.current.ids, edited_text: delivery.current.editedText });
      for (const id of delivery.current.ids) delivered.current.add(id);
      delivery.current = null; editIds.current = null; setEditedText(null); save(`voice-delivery:${agentId}`, null); setSent([...delivered.current]); setStatus("Sent. Waiting for the orchestrator.");
    } catch (e) { setStatus(errorText(e)); } finally { setBusy(false); }
  };
  const sendRef = useRef(send); sendRef.current = send;
  const playback = useRef<RecordRow | null>(null);
  const stopAudio = () => {
    speech.current?.pause(); speech.current = null;
    if (playback.current) void event("playback", "interrupted", { record_id: playback.current.id }).catch(() => {});
    playback.current = null;
  };
  const pump = async () => {
    if (!live.current || speaking.current || responding.current || playback.current || !queue.current.length) return;
    const row = queue.current.shift()!; playback.current = row;
    try {
      const result = await post("speech", { record_id: row.id, session_id: session.current });
      if (!live.current || playback.current?.id !== row.id) return;
      if (speaking.current || responding.current) { queue.current.unshift(row); playback.current = null; return; }
      const audio = new Audio(`data:${result.mime};base64,${result.audio}`); speech.current = audio;
      audio.onended = () => { speech.current = null; if (row.id.startsWith("approval-speech:")) lastPermission.current = row.id; else lastPermission.current = ""; void event("playback", "played", { record_id: row.id }); playback.current = null; void pump(); };
      audio.onerror = () => { void event("playback", "unknown", { record_id: row.id }); playback.current = null; setStatus("Audio failed. Use Repeat."); };
      await audio.play(); void event("playback", "playing", { record_id: row.id });
    } catch (e) { playback.current = null; setStatus(`Audio: ${errorText(e)}`); }
  };
  const close = () => {
    generation.current++;
    live.current = false; stopAudio(); queue.current = [];
    if (recorder.current?.state === "recording") recorder.current.stop(); recorder.current = null;
    channel.current?.close(); pc.current?.close(); stream.current?.getTracks().forEach(t => t.stop());
    remote.current?.pause(); if (remote.current) remote.current.srcObject = null;
    pc.current = null; stream.current = null; channel.current = null;
    if (session.current) void post("end", { session_id: session.current }).catch(() => {});
    setActive(false); setBusy(false); setMuted(false); setStatus("");
  };
  useEffect(() => {
    let disposed = false;
    void (async () => {
      for (const body of [...pendingRecords.current]) {
        await post("record", body);
        pendingRecords.current = pendingRecords.current.filter(r => r.event_id !== body.event_id);
        save(`voice-pending:${agentId}`, pendingRecords.current);
      }
      return post("records");
    })().then((data: Records) => {
      if (disposed) return;
      all.current = data.records; delivered.current = new Set(data.delivered); cursor.current = data.cursor;
      setRows(data.records); setSent(data.delivered);
    }).catch(e => { if (!disposed && pendingRecords.current.length) setStatus(errorText(e)); });
    return () => { disposed = true; close(); };
  }, [agentId]);
  useEffect(() => {
    if (!active) return;
    let disposed = false, fetching = false;
    const timer = window.setInterval(async () => {
      if (fetching) return; fetching = true;
      try {
        const data: Records = await post("records", { after: cursor.current });
        const approvalData = await post("approvals");
        if (!disposed) setApprovals(approvalData.requests);
        if (disposed) return;
        cursor.current = data.cursor;
        for (const row of data.records) {
          if (!all.current.some(r => r.id === row.id)) all.current.push(row);
          if (row.kind === "orchestrator") { queue.current.push(row); void event("playback", "queued", { record_id: row.id }); }
        }
        setRows([...all.current]); void pump();
      } catch (e) { if (!disposed) setStatus(`Sync: ${errorText(e)}`); } finally { fetching = false; }
    }, 1200);
    return () => { disposed = true; clearInterval(timer); };
  }, [active]);
  const start = async () => {
    const ticket = ++generation.current;
    setBusy(true); setStatus("Connecting…");
    try {
      const config = await post("status");
      if (ticket !== generation.current) return;
      if (!config.configured) throw new Error(config.setup);
      if (!window.isSecureContext || !navigator.mediaDevices) throw new Error("Voice needs HTTPS. Open the Tailscale HTTPS address.");
      session.current = crypto.randomUUID(); pending.current.clear(); order.current.clear(); previous.current.clear(); serial.current = Promise.resolve();
      const connection = new RTCPeerConnection(); pc.current = connection;
      remote.current = new Audio(); remote.current.autoplay = true;
      connection.ontrack = e => { if (remote.current) { remote.current.srcObject = e.streams[0]; void remote.current.play().catch(() => setStatus("Tap Resume audio to enable sound.")); } };
      stream.current = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true } });
      if (ticket !== generation.current) { stream.current.getTracks().forEach(t => t.stop()); return; }
      stream.current.getTracks().forEach(track => connection.addTrack(track, stream.current!));
      const dc = connection.createDataChannel("oai-events"); channel.current = dc;
      dc.onmessage = message => {
        const e = JSON.parse(message.data);
        if (e.type === "input_audio_buffer.speech_started") { speaking.current = true; pending.current.add("awaiting-commit"); if (speech.current) stopAudio(); }
        if (e.type === "input_audio_buffer.speech_stopped") speaking.current = false;
        if (e.type === "input_audio_buffer.committed") { previous.current.set(e.item_id, e.previous_item_id || ""); pending.current.delete("awaiting-commit"); pending.current.add(e.item_id); if (!order.current.has(e.item_id)) order.current.set(e.item_id, order.current.size); }
        if (["conversation.item.added", "conversation.item.created"].includes(e.type) && e.item?.id && !order.current.has(e.item.id)) order.current.set(e.item.id, order.current.size);
        if (e.type === "response.created") responding.current = true;
        if (e.type === "conversation.item.input_audio_transcription.completed" || e.type === "response.output_audio_transcript.done") {
          const user = e.type.startsWith("conversation");
          if (!order.current.has(e.item_id)) order.current.set(e.item_id, order.current.size);
          void event(user ? "user" : "courier", e.transcript, { utterance_order: order.current.get(e.item_id) }, `${session.current}:${e.item_id}:${user ? "user" : "courier"}:${e.content_index ?? 0}`, e.item_id, previous.current.get(e.item_id) || "").then(() => {
            if (user) {
              pending.current.delete(e.item_id);
              if (e.transcript.trim().toLowerCase().replace(/[.!?]+$/, "") === "разрешаю" && lastPermission.current) {
                void post("approve", { session_id: session.current, speech_id: lastPermission.current, transcript_id: `${session.current}:${e.item_id}:user:${e.content_index ?? 0}` }).then(() => setStatus("Permission accepted.")).catch(error => setStatus(errorText(error)));
                lastPermission.current = "";
              }
            }
            if (sendWanted.current) void sendRef.current();
          }).catch(() => {});
        }
        if (e.type === "response.function_call_arguments.done" && e.name === "send_transcript") {
          sendWanted.current = true; voiceSend.current = true;
          dc.send(JSON.stringify({ type: "conversation.item.create", item: { type: "function_call_output", call_id: e.call_id, output: JSON.stringify({ status: "The app will send after transcription finishes. Do not claim delivery." }) } }));
        }
        if (e.type === "response.done") { responding.current = false; if (sendWanted.current) void sendRef.current(); void pump(); }
        if (e.type === "error" || e.type === "conversation.item.input_audio_transcription.failed") {
          setStatus(e.error?.message || "Voice transcription failed. Do not send until the missing speech is repeated.");
          void event("event", "error", e).catch(() => {});
        }
      };
      connection.onconnectionstatechange = () => { if (["failed", "disconnected"].includes(connection.connectionState)) { close(); setStatus("Voice disconnected. Start again. The saved transcript remains."); } };
      dc.onopen = () => {
        const history = all.current.filter(r => ["user", "courier", "orchestrator"].includes(r.kind));
        for (const row of history) dc.send(JSON.stringify({ type: "conversation.item.create", item: { type: "message", role: row.kind === "user" ? "user" : "assistant", content: [{ type: row.kind === "user" ? "input_text" : "text", text: row.text }] } }));
      };
      const offer = await connection.createOffer(); await connection.setLocalDescription(offer);
      const answer = await post("start", { session_id: session.current, sdp: offer.sdp });
      if (ticket !== generation.current) { void post("end", { session_id: session.current }); return; }
      cursor.current = answer.cursor; all.current = answer.records; setRows(answer.records); delivered.current = new Set(answer.delivered); setSent(answer.delivered);
      await connection.setRemoteDescription({ type: "answer", sdp: answer.sdp });
      if (typeof MediaRecorder !== "undefined" && stream.current) {
        const mime = ["audio/mp4", "audio/webm;codecs=opus", "audio/webm"].find(t => MediaRecorder.isTypeSupported(t));
        if (mime) {
          const captureSession = session.current;
          const capture = new MediaRecorder(stream.current, { mimeType: mime }); recorder.current = capture;
          capture.ondataavailable = e => {
            if (!e.data.size) return;
            const chunk_id = crypto.randomUUID();
            const reader = new FileReader();
            reader.onload = () => { void post("audio", { session_id: captureSession, chunk_id, audio: String(reader.result).split(",")[1], mime }).catch(error => setStatus(`Audio debug copy: ${errorText(error)}`)); };
            reader.readAsDataURL(e.data);
          };
          capture.start(15000);
        }
      }
      live.current = true; setActive(true); setStatus("Listening. Send with the button or an explicit voice command.");
    } catch (e) { close(); setStatus(errorText(e)); notify(errorText(e)); } finally { setBusy(false); }
  };
  return <section className="realtime-voice" aria-label="Voice courier">
    <div className="realtime-voice-actions">
      <button type="button" disabled={busy} onClick={() => active ? close() : void start()}>{active ? "End voice" : "Voice"}</button>
      {active && <><button type="button" onClick={() => { const next = !muted; stream.current?.getAudioTracks().forEach(t => { t.enabled = !next; }); setMuted(next); }}>{muted ? "Enable microphone" : "Mute"}</button><button type="button" onClick={() => void remote.current?.play()}>Resume audio</button></>}
      {(active || rows.some(r => ["user", "courier"].includes(r.kind) && !sent.includes(r.id))) && <button type="button" disabled={busy || !rows.some(r => ["user", "courier"].includes(r.kind) && !sent.includes(r.id))} onClick={() => { voiceSend.current = false; void send(); }}>Send transcript</button>}
    </div>
    {active && approvals.map(request => <button type="button" key={request.id} onClick={() => { void post("approval_speech", { request_id: request.id }).then(() => setStatus("The permission will be read next. Say разрешаю after it ends.")).catch(e => setStatus(errorText(e))); }}>Read permission: {request.method} ({request.id.slice(0, 8)})</button>)}
    {rows.some(r => ["user", "courier"].includes(r.kind) && !sent.includes(r.id)) && <details><summary>Edit transcript before send</summary><textarea aria-label="Transcript to send" rows={5} value={editedText ?? ("Voice conversation, full transcript (speech recognition can contain errors):\n\n" + orderedVoiceRecords(rows).filter(r => ["user", "courier"].includes(r.kind) && !sent.includes(r.id)).map(r => `${r.kind === "user" ? "User" : "Voice courier"}: ${r.text}`).join("\n\n"))} onChange={e => { editIds.current ??= rows.filter(r => ["user", "courier"].includes(r.kind) && !sent.includes(r.id)).map(r => r.id); setEditedText(e.target.value); }} /><button type="button" onClick={() => { editIds.current = null; setEditedText(null); }}>Restore original transcript</button><small>The original remains in history. Speech after you start edits stays for the next send.</small></details>}
    {status && <p role="status">{status}</p>}
    {!!pendingRecords.current.length && <button type="button" onClick={() => { void (async () => { for (const body of [...pendingRecords.current]) { const row = await post("record", body); if (["user", "courier"].includes(row.kind) && !all.current.some(r => r.id === row.id)) { all.current.push(row); setRows([...all.current]); } pendingRecords.current = pendingRecords.current.filter(r => r.event_id !== body.event_id); save(`voice-pending:${agentId}`, pendingRecords.current); } setStatus("Transcript sync restored."); })().catch(e => setStatus(errorText(e))); }}>Retry transcript sync</button>}
    {active && <small>AI-generated voice. Audio goes to OpenAI. Transcripts remain on your Mac.</small>}
    {!!rows.length && <details><summary>Voice transcript and speech</summary><div className="realtime-voice-transcript">{orderedVoiceRecords(rows).filter(r => r.text && ["user", "courier", "orchestrator"].includes(r.kind)).map(r => <div key={r.id}><strong>{r.kind === "user" ? "You" : r.kind === "courier" ? "Voice courier" : "Orchestrator"}</strong><p>{r.text}</p>{r.kind === "orchestrator" && active && <button type="button" onClick={() => { queue.current.push(r); void pump(); }}>Repeat</button>}</div>)}</div></details>}
  </section>;
}
