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

type Schemas = components["schemas"];
type VoiceCallBody = Partial<Schemas["VoiceStartRequest"]>;
type VoiceHarness = {
  calls: [string, VoiceCallBody][];
  order: string[];
  stops: number;
  records: Pick<
    Schemas["VoiceRecordResponse"],
    "id" | "seq" | "kind" | "text"
  >[];
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
  answers?: number;
  track: Pick<MediaStreamTrack, "enabled" | "stop">;
  emit: (event: { type: string; item?: { id: string } }) => void;
  resourceChange: () => Promise<Response>;
  render: (id: string) => void;
  disconnect: () => void;
};
declare global {
  interface Window {
    voiceCalls: VoiceHarness["calls"];
    order: VoiceHarness["order"];
    stops: VoiceHarness["stops"];
    records: VoiceHarness["records"];
    pending: VoiceHarness["pending"];
    voiceError: VoiceHarness["voiceError"];
    voiceTransport: VoiceHarness["voiceTransport"];
    holdPermission: VoiceHarness["holdPermission"];
    releasePermission: VoiceHarness["releasePermission"];
    holdStart: VoiceHarness["holdStart"];
    releaseStart: VoiceHarness["releaseStart"];
    failRecords: VoiceHarness["failRecords"];
    sid: VoiceHarness["sid"];
    answer: VoiceHarness["answer"];
    answers: VoiceHarness["answers"];
    track: VoiceHarness["track"];
    emit: VoiceHarness["emit"];
    resourceChange: VoiceHarness["resourceChange"];
    render: VoiceHarness["render"];
    disconnect: VoiceHarness["disconnect"];
  }
}
type FixtureTrack = VoiceHarness["track"];
type FixtureChannel = Pick<RTCDataChannel, "close" | "send"> & {
  onmessage?: (event: Pick<MessageEvent, "data">) => void;
};
type FixturePeer = Pick<
  RTCPeerConnection,
  "addTrack" | "close" | "setLocalDescription" | "setRemoteDescription"
> & {
  createDataChannel(
    ...args: Parameters<RTCPeerConnection["createDataChannel"]>
  ): FixtureChannel;
  createOffer(): Promise<Pick<RTCSessionDescriptionInit, "sdp">>;
  connectionState?: RTCPeerConnectionState;
  onconnectionstatechange?: () => void;
};
// The base fixture stores minimal DOM stubs on window; this single typed view
// preserves those same objects without creating real audio or RTC resources.
type FixtureWindow = Omit<Window, "RTCPeerConnection" | "codexDesktop"> & {
  RTCPeerConnection: new () => FixturePeer;
  codexDesktop: {
    requestMicrophone: (
      ...args: Parameters<
        import("../../src/desktop.ts").DesktopBridge["requestMicrophone"]
      >
    ) => Promise<void>;
  };
  pc?: FixturePeer;
  dc?: FixtureChannel;
};
function voiceWindow(value: unknown): FixtureWindow {
  return value as FixtureWindow;
}
const fixtureWindow = voiceWindow(window);
const nativeFetch = window.fetch.bind(window);
const root = createRoot(fixtureRoot());
window.voiceCalls = [];
window.order = [];
window.stops = 0;
window.records = [];
window.pending = false;
window.voiceError = "";
window.track = undefined!;
window.emit = function (event) {
  fixtureWindow.dc!.onmessage!({ data: JSON.stringify(event) });
};
window.resourceChange = () =>
  nativeFetch("/audit/resource-change", { method: "POST" });
window.render = (id) =>
  root.render(
    <MantineProvider defaultColorScheme="dark">
      <RealtimeVoice agentId={id} notify={() => {}} />
    </MantineProvider>,
  );
window.disconnect = function () {
  fixtureWindow.pc!.connectionState = "disconnected";
  fixtureWindow.pc!.onconnectionstatechange!();
};
watchVoiceTransport(
  (state) => {
    window.voiceTransport = state;
  },
  () => {},
);
fixtureWindow.codexDesktop = {
  requestMicrophone: async () => {
    window.order.push("permission");
    if (window.holdPermission)
      await new Promise<void>((resolve) => {
        window.releasePermission = resolve;
      });
  },
};
fixtureWindow.RTCPeerConnection = class {
  constructor() {
    fixtureWindow.pc = this;
  }
  addTrack() {
    return undefined!;
  }
  createDataChannel() {
    return (fixtureWindow.dc = {
      close() {},
      send() {
        throw Error("Native Core owns messages");
      },
    });
  }
  async createOffer() {
    return { sdp: "v=0\\noffer" };
  }
  async setLocalDescription() {}
  async setRemoteDescription(sdp: RTCSessionDescriptionInit) {
    window.answers = (window.answers || 0) + 1;
    window.answer = sdp;
    window.emit({ type: "session.started" });
  }
  close() {}
};
const track = (window.track = {
  enabled: true,
  stop() {
    window.stops++;
  },
});
Object.defineProperty(navigator, "mediaDevices", {
  value: {
    getUserMedia: async () => {
      window.order.push("capture");
      return {
        getTracks: () => [track],
        getAudioTracks: () => [track],
      } satisfies {
        [K in "getTracks" | "getAudioTracks"]: () => FixtureTrack[];
      };
    },
  },
});

function session(): Pick<
  Schemas["VoiceSessionResponse"],
  "session_id" | "state" | "sdp" | "error"
> {
  return {
    session_id: window.sid!,
    state: window.voiceError
      ? "failed"
      : window.pending
        ? "connecting"
        : "ready",
    sdp: window.pending ? null : "answer",
    error: window.voiceError,
  };
}
/** The renderer under test owns the voice JSON request bodies. */
function voiceBody(body: string): VoiceCallBody {
  return JSON.parse(body || "{}") as VoiceCallBody;
}
window.fetch = async (input, opts) => {
  const request = input instanceof Request ? input : null;
  const path = request?.url || input;
  const body = request ? await request.clone().text() : opts?.body;
  const action = new URL(path as string | URL, location.origin).pathname
    .split("/")
    .at(-1)!;
  const payload = voiceBody(body as string);
  window.voiceCalls.push([action, payload]);
  let data: unknown = {};
  if (action === "identity")
    data = {
      workspaceId: "0123456789abcdef0123456789abcdef",
      syncProtocol: 2,
    } satisfies Schemas["SyncIdentityResponse"];
  if (action === "status") {
    window.order.push("status");
    data = {
      configured: true,
      transport: "native",
    } satisfies Pick<
      Schemas["VoiceStatusResponse"],
      "configured" | "transport"
    >;
  }
  if (action === "records" && window.failRecords)
    throw new Error("Connection unavailable");
  if (action === "records")
    data = {
      records: window.records,
      cursor: window.records.length,
      session: window.sid ? session() : null,
    } satisfies {
      records: VoiceHarness["records"];
      cursor: Schemas["VoiceRecordsResponse"]["cursor"];
      session: ReturnType<typeof session> | null;
    };
  if (action === "start") {
    window.sid = payload.session_id;
    const stale = window.holdStart
      ? ({
          session_id: window.sid!,
          state: "connecting",
          sdp: null,
        } satisfies Pick<
          Schemas["VoiceSessionResponse"],
          "session_id" | "state" | "sdp"
        >)
      : null;
    if (window.holdStart)
      await new Promise<void>((resolve) => {
        window.releaseStart = resolve;
      });
    data = stale || {
      session_id: window.sid,
      state: window.pending ? "connecting" : "ready",
      sdp: window.pending ? null : "answer",
    };
  }
  if (action === "session") data = session();
  return new Response(JSON.stringify(data), {
    headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH },
  });
};
window.render("chat-one");
