import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import RealtimeVoice from "../../src/components/RealtimeVoice.tsx";
import { watchVoiceTransport } from "../../src/components/voiceTransport.ts";
import type { ResourceConnectionState } from "../../src/sync/client.ts";
import type { components } from "../../src/generated/api.ts";
import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../../src/generated/apiSchema.ts";
import { fixtureRoot } from "../usage/usage-fixture.ts";
import { desktopBridgeFixture } from "../desktop-bridge-fixture.ts";

type Schemas = components["schemas"];
type VoiceCallBody = Partial<Schemas["VoiceStartRequest"]>;
type VoiceHarness = {
  calls: [string, VoiceCallBody][];
  order: string[];
  stops: number;
  records: Schemas["VoiceRecordResponse"][];
  pending: boolean;
  voiceError: string;
  voiceTransport?: ResourceConnectionState;
  holdPermission?: boolean;
  releasePermission?: () => void;
  holdStart?: boolean;
  releaseStart?: () => void;
  failRecords?: boolean;
  sid?: string | undefined;
  answer?: RTCSessionDescriptionInit;
  answers: number;
  track: MediaStreamTrack;
  emit: (event: { type: string; item?: { id: string } }) => void;
  resourceChange: () => Promise<Response>;
  render: (id: string) => void;
  disconnect: () => void;
};
declare global {
  interface Window {
    realtimeVoiceFixture: VoiceHarness;
  }
}
const audio = new AudioContext();
const stream = audio.createMediaStreamDestination().stream;
const track = stream.getAudioTracks()[0];
if (!track) throw new Error("Voice fixture needs an audio track");
const nativeFetch = window.fetch.bind(window);
const root = createRoot(fixtureRoot());
const harness: VoiceHarness = (window.realtimeVoiceFixture = {
  calls: [],
  order: [],
  stops: 0,
  records: [],
  pending: false,
  voiceError: "",
  answers: 0,
  track,
  emit(event) {
    channel?.dispatchEvent(
      new MessageEvent("message", { data: JSON.stringify(event) }),
    );
  },
  resourceChange: () =>
    nativeFetch("/audit/resource-change", { method: "POST" }),
  render: (id) =>
    root.render(
      <MantineProvider defaultColorScheme="dark">
        <RealtimeVoice agentId={id} notify={() => {}} />
      </MantineProvider>,
    ),
  disconnect() {},
});
watchVoiceTransport(
  (state) => {
    harness.voiceTransport = state;
  },
  () => {},
);
window.codexDesktop = desktopBridgeFixture({
  requestMicrophone: async () => {
    harness.order.push("permission");
    if (harness.holdPermission)
      await new Promise<void>((resolve) => {
        harness.releasePermission = resolve;
      });
    return true;
  },
});
let channel: RTCDataChannel | undefined;

class FixturePeer extends RTCPeerConnection {
  fixtureConnectionState: RTCPeerConnectionState = "new";
  constructor(...args: ConstructorParameters<typeof RTCPeerConnection>) {
    super(...args);
    harness.disconnect = () => {
      this.fixtureConnectionState = "disconnected";
      this.dispatchEvent(new Event("connectionstatechange"));
    };
  }
  override get connectionState() {
    return this.fixtureConnectionState;
  }
  override createDataChannel(label: string, options?: RTCDataChannelInit) {
    const result = super.createDataChannel(label, options);
    result.send = () => {
      throw new Error("Native Core owns messages");
    };
    channel = result;
    return result;
  }
  override createOffer(
    options?: RTCOfferOptions,
  ): Promise<RTCSessionDescriptionInit>;
  override createOffer(
    success: RTCSessionDescriptionCallback,
    failure: RTCPeerConnectionErrorCallback,
    options?: RTCOfferOptions,
  ): Promise<void>;
  override async createOffer(
    first?: RTCOfferOptions | RTCSessionDescriptionCallback,
  ): Promise<RTCSessionDescriptionInit | void> {
    const offer: RTCSessionDescriptionInit = {
      type: "offer",
      sdp: "v=0\\noffer",
    };
    if (typeof first === "function") {
      first(offer);
      return;
    }
    return offer;
  }
  override async setLocalDescription(): Promise<void> {}
  override async setRemoteDescription(
    sdp: RTCSessionDescriptionInit,
  ): Promise<void> {
    harness.answers++;
    harness.answer = sdp;
    harness.emit({ type: "session.started" });
  }
}
window.RTCPeerConnection = FixturePeer;
// The fixture uses a real browser track and stream without microphone capture.
track.stop = () => {
  harness.stops++;
};
Object.defineProperty(navigator, "mediaDevices", {
  value: {
    getUserMedia: async () => {
      harness.order.push("capture");
      return stream;
    },
  } satisfies Pick<MediaDevices, "getUserMedia">,
});

function session(): Schemas["VoiceSessionResponse"] {
  return {
    session_id: harness.sid ?? "",
    state: harness.voiceError
      ? "failed"
      : harness.pending
        ? "connecting"
        : "ready",
    sdp: harness.pending ? null : "answer",
    error: harness.voiceError,
    ended: null,
  };
}
/** The renderer under test owns the voice JSON request bodies. */
function voiceBody(body: string): VoiceCallBody {
  return JSON.parse(body || "{}") as VoiceCallBody;
}
window.fetch = async (input, opts) => {
  const request = input instanceof Request ? input : null;
  const path =
    request?.url ?? (input instanceof URL ? input.href : String(input));
  const body = request
    ? await request.clone().text()
    : typeof opts?.body === "string"
      ? opts.body
      : "";
  const action =
    new URL(path, location.origin).pathname.split("/").at(-1) ?? "";
  const payload = voiceBody(body);
  harness.calls.push([action, payload]);
  let data: unknown = {};
  if (action === "identity")
    data = {
      workspaceId: "0123456789abcdef0123456789abcdef",
      syncProtocol: 2,
    } satisfies Schemas["SyncIdentityResponse"];
  if (action === "status") {
    harness.order.push("status");
    data = {
      configured: true,
      transport: "native",
      auth: "chatgpt",
    } satisfies Schemas["VoiceStatusResponse"];
  }
  if (action === "records" && harness.failRecords)
    throw new Error("Connection unavailable");
  if (action === "records")
    data = {
      records: harness.records,
      cursor: harness.records.length,
      delivered: [],
      session: harness.sid ? session() : null,
    } satisfies Schemas["VoiceRecordsResponse"];
  if (action === "start") {
    harness.sid = payload.session_id;
    const stale = harness.holdStart
      ? ({
          ...session(),
          state: "connecting",
          sdp: null,
        } satisfies Schemas["VoiceSessionResponse"])
      : null;
    if (harness.holdStart)
      await new Promise<void>((resolve) => {
        harness.releaseStart = resolve;
      });
    data = stale ?? session();
  }
  if (action === "session") data = session();
  return new Response(JSON.stringify(data), {
    headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH },
  });
};
harness.render("chat-one");
