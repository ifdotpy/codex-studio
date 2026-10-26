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
    realtimeVoiceFixture: VoiceHarness;
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
const harness: VoiceHarness = (window.realtimeVoiceFixture = {
  calls: [],
  order: [],
  stops: 0,
  records: [],
  pending: false,
  voiceError: "",
  track: undefined!,
  emit(event) {
    fixtureWindow.dc!.onmessage!({ data: JSON.stringify(event) });
  },
  resourceChange: () =>
    nativeFetch("/audit/resource-change", { method: "POST" }),
  render: (id) =>
    root.render(
      <MantineProvider defaultColorScheme="dark">
        <RealtimeVoice agentId={id} notify={() => {}} />
      </MantineProvider>,
    ),
  disconnect() {
    fixtureWindow.pc!.connectionState = "disconnected";
    fixtureWindow.pc!.onconnectionstatechange!();
  },
});
watchVoiceTransport(
  (state) => {
    harness.voiceTransport = state;
  },
  () => {},
);
fixtureWindow.codexDesktop = {
  requestMicrophone: async () => {
    harness.order.push("permission");
    if (harness.holdPermission)
      await new Promise<void>((resolve) => {
        harness.releasePermission = resolve;
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
    harness.answers = (harness.answers || 0) + 1;
    harness.answer = sdp;
    harness.emit({ type: "session.started" });
  }
  close() {}
};
const track = (harness.track = {
  enabled: true,
  stop() {
    harness.stops++;
  },
});
Object.defineProperty(navigator, "mediaDevices", {
  value: {
    getUserMedia: async () => {
      harness.order.push("capture");
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
    session_id: harness.sid!,
    state: harness.voiceError
      ? "failed"
      : harness.pending
        ? "connecting"
        : "ready",
    sdp: harness.pending ? null : "answer",
    error: harness.voiceError,
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
    } satisfies Pick<
      Schemas["VoiceStatusResponse"],
      "configured" | "transport"
    >;
  }
  if (action === "records" && harness.failRecords)
    throw new Error("Connection unavailable");
  if (action === "records")
    data = {
      records: harness.records,
      cursor: harness.records.length,
      session: harness.sid ? session() : null,
    } satisfies {
      records: VoiceHarness["records"];
      cursor: Schemas["VoiceRecordsResponse"]["cursor"];
      session: ReturnType<typeof session> | null;
    };
  if (action === "start") {
    harness.sid = payload.session_id;
    const stale = harness.holdStart
      ? ({
          session_id: harness.sid!,
          state: "connecting",
          sdp: null,
        } satisfies Pick<
          Schemas["VoiceSessionResponse"],
          "session_id" | "state" | "sdp"
        >)
      : null;
    if (harness.holdStart)
      await new Promise<void>((resolve) => {
        harness.releaseStart = resolve;
      });
    data = stale || {
      session_id: harness.sid,
      state: harness.pending ? "connecting" : "ready",
      sdp: harness.pending ? null : "answer",
    };
  }
  if (action === "session") data = session();
  return new Response(JSON.stringify(data), {
    headers: { [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH },
  });
};
harness.render("chat-one");
