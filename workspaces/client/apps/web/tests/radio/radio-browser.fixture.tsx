declare global {
  interface Window {
    radioFixture: {
      radio: () => NonNullable<Room["radio"]>;
      setRoom: React.Dispatch<React.SetStateAction<Room>>;
      currentRoom: Room;
      opened: string | null;
      refreshCalls: number;
      holdRefresh: boolean;
      releaseRefresh: () => void;
      rejectRefresh: (reason: Error) => void;
      failRefresh: boolean;
      notifications: string[];
    };
  }
}
const rootElement = document.getElementById("root");
if (!rootElement) throw new Error("Missing radio fixture root");
import { snapshotFixture } from "../../src/snapshotFixture.ts";
import type { Room, Snapshot, PeerTeam } from "../../src/types.ts";
import { radioRoom } from "./radio-fixture.ts";

import React, { useCallback, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import RadioChat from "../../src/components/RadioChat.tsx";
import { PeerTeamGroup } from "../../src/components/shell/messages/PeerTeams.tsx";
import "../../src/style.css";
import "../../src/appearance.css";
import { theme } from "../../src/theme.ts";
const team: PeerTeam = {
  id: "t",
  name: "Research",
  projectPath: "/p",
  members: ["a", "b"],
};
const room: Room = {
  id: "radio:t",
  name: "Research",
  kind: "private",
  projectPath: "/p",
  peerTeamId: "t",
  members: ["a", "b"],
  radio: {
    teamId: "t",
    revision: 1,
    status: "idle",
    speaker: null,
    next: [],
    active: null,
    error: null,
  },
};
const initial: Snapshot = {
  ...snapshotFixture(),
  stateDir: "radio-test",
  threads: [
    { id: "a", name: "Astra", model: "gpt-6-astra", provider: "codex" },
    { id: "b", name: "Claude", model: "claude-opus-5-5", provider: "claude" },
  ],
  runtime: { ...snapshotFixture().runtime, requests: [], peerTeams: [team] },
};
window.radioFixture = {
  radio: () => {
    const state = window.radioFixture.currentRoom.radio;
    if (!state) throw new Error("Missing radio state");
    return state;
  },
  setRoom: () => {},
  currentRoom: room,
  opened: null,
  refreshCalls: 0,
  holdRefresh: false,
  releaseRefresh: () => {},
  rejectRefresh: () => {},
  failRefresh: false,
  notifications: [],
};
function Fixture() {
  const [data] = useState<Snapshot>(initial);
  const [value, setRoom] = useState(room);
  const drafts = useRef<Record<string, string>>({});
  const listeners = useRef(new Map<string, Set<() => void>>());
  const getDraft = useCallback(
    (session: string) => drafts.current[session] || "",
    [],
  );
  const subscribeDraft = useCallback(
    (session: string, listener: () => void) => {
      let current = listeners.current.get(session);
      if (!current) listeners.current.set(session, (current = new Set()));
      current.add(listener);
      return () => {
        current.delete(listener);
        if (!current.size) listeners.current.delete(session);
      };
    },
    [],
  );
  const setDraft = (
    value: string | ((before: string) => string),
    session = room.id,
  ) => {
    const previous = drafts.current[session] || "";
    const text = typeof value === "function" ? value(previous) : value;
    drafts.current = { ...drafts.current, [session]: text };
    if (text !== previous)
      listeners.current.get(session)?.forEach((listener) => listener());
  };
  const [roomId, setRoomId] = useState<string | undefined>(undefined);
  window.radioFixture.setRoom = setRoom;
  window.radioFixture.currentRoom = value;
  window.radioFixture.opened = null;
  const refresh = async () => {
    window.radioFixture.refreshCalls =
      (window.radioFixture.refreshCalls || 0) + 1;
    if (window.radioFixture.holdRefresh)
      await new Promise<void>((resolve, reject) => {
        window.radioFixture.releaseRefresh = resolve;
        window.radioFixture.rejectRefresh = reject;
      });
    if (window.radioFixture.failRefresh) throw Error("Refresh failed");
    const response = await fetch("/snapshot");
    setRoom(radioRoom(await response.json()));
    setRoomId("radio:t");
  };
  return (
    <MantineProvider theme={theme} defaultColorScheme="dark">
      <div
        style={{
          height: "100%",
          display: "flex",
          flexDirection: "column",
          width: "100%",
        }}
      >
        <PeerTeamGroup
          scope="radio-test"
          team={team}
          closed={false}
          toggle={() => {}}
          edit={() => {}}
          dissolve={() => {}}
          roomId={roomId}
          openRoom={(id) => (window.radioFixture.opened = id)}
          refresh={refresh}
        >
          <></>
        </PeerTeamGroup>
        <RadioChat
          room={value}
          data={data}
          getDraft={getDraft}
          subscribeDraft={subscribeDraft}
          setDraft={setDraft}
          refresh={refresh}
          notify={(error) =>
            (window.radioFixture.notifications ??= []).push(error)
          }
        />
      </div>
    </MantineProvider>
  );
}
createRoot(rootElement).render(<Fixture />);
