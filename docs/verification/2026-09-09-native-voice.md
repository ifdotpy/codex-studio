# Native voice, 2026-09-09

Studio uses Codex 0.153.4 v3 WebRTC through the selected ChatGPT account.
The separate REST voice and text-to-speech paths are removed.
Native Core owns voice delegation and the return of lead responses.
Studio does not send a second user message from a transcript.

## Evidence

- Sixteen deterministic lifecycle tests cover request identity, account isolation,
  delayed answers, cancellation, reconnect boundaries, speech receipts, and history.
- The existing persistence and legacy draft validation checks pass.
- Browser fixtures cover native microphone permission, delayed SDP, connection
  failure, cancellation during permission and start, and chat changes.
- An installed app-server test enables realtime on an idle persisted thread after
  unsubscribe and resume. A loaded thread with background commands is not reloaded.
- A live ChatGPT account returned the WebRTC answer. Headless Chrome connected,
  opened the data channel, and received a remote audio track.
- A synthetic audio recording asked Codex to inspect its current directory.
  The native service emitted transcription, started a Codex turn, and completed it.
  Studio saved the recognized user text. Ending voice reached `ended`.
- These probes used an isolated ephemeral thread. They did not restart user agents.

The v3 service emits transcript delta/done events in the observed session.
Studio saves completed text and retains an incomplete tail on disconnect.
Canonical item events are also supported when supplied by the native service.

## Limits

The live audio source was a generated recording, not the user's microphone.
Physical speaker playback and the user's microphone remain unverified.
A speech submission receipt does not prove exact audible playback.
Native voice may delegate before the speaker requests a separate transcript send.
Permission decisions remain in the chat interface.
