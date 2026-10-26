import { fixtureElement } from "./browser-helpers.ts";

import React, { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { useMessages } from "../../src/hooks.ts";
import { createServerServices } from "../../src/servers/serverServices.ts";
import { ServerServicesProvider } from "../../src/servers/ServerServicesContext.tsx";
const services = createServerServices({
  id: "local",
  label: "Fixture",
  origin: location.origin,
});
await services.ready();
window.studioChatSwitchRecoveryBrowserFixtureServices = services;
window.studioChatSwitchRecoveryBrowserFixtureCursorReads = [];
window.studioChatSwitchRecoveryBrowserFixtureConnections = [];
window.studioChatSwitchRecoveryBrowserFixturePanelConnections = [];
window.studioChatSwitchRecoveryBrowserFixtureDocumentIdentity =
  crypto.randomUUID();
const readCursor = services.sync.readSubscriptionCursor;
services.sync.readSubscriptionCursor = async (ref) => {
  if (window.studioChatSwitchRecoveryBrowserFixtureSlowCursor) {
    window.studioChatSwitchRecoveryBrowserFixtureCursorReads.push({
      ref,
      at: performance.now(),
    });
    // Slow cache I/O crosses the real owner's three-second heartbeat.
    await new Promise((resolve) => setTimeout(resolve, 4000));
  }
  return readCursor(ref);
};
services.resources.watchResourceConnection((status, stream) =>
  window.studioChatSwitchRecoveryBrowserFixtureConnections.push({
    status,
    stream,
    at: performance.now(),
  }),
);
function Panel({ id, selected }: { id: string; selected: boolean }) {
  const messages = useMessages(
    id,
    "agent",
    true,
    "fixture",
    "e".repeat(32),
    selected,
  );
  useEffect(() => {
    window.studioChatSwitchRecoveryBrowserFixturePanelConnections.push({
      id,
      selected,
      connection: messages.connection,
      at: performance.now(),
    });
  }, [id, selected, messages.connection]);
  return React.createElement(
    "section",
    { hidden: !selected, "data-chat": id },
    React.createElement(
      "output",
      { "data-connection": true },
      messages.connection?.health ?? "",
    ),
    React.createElement(
      "div",
      { "data-messages": true },
      messages.items.map((item) => item.text).join("\n"),
    ),
  );
}
function Fixture() {
  const [selected, setSelected] = useState("first");
  return React.createElement(
    React.Fragment,
    null,
    React.createElement(
      "nav",
      { "aria-label": "Chats" },
      ["first", "second", "third"].map((id) =>
        React.createElement(
          "button",
          { key: id, onClick: () => setSelected(id) },
          id,
        ),
      ),
    ),
    ["first", "second", "third"].map((id) =>
      React.createElement(Panel, { key: id, id, selected: selected === id }),
    ),
  );
}
createRoot(fixtureElement("app")).render(
  <ServerServicesProvider services={services}>
    <Fixture />
  </ServerServicesProvider>,
);

declare global {
  interface Window {
    studioChatSwitchRecoveryBrowserFixtureServices: ReturnType<
      typeof import("../../src/servers/serverServices.ts").createServerServices
    >;
    studioChatSwitchRecoveryBrowserFixtureDocumentIdentity: string;
    studioChatSwitchRecoveryBrowserFixtureSlowCursor: boolean;
    studioChatSwitchRecoveryBrowserFixtureCursorReads: {
      ref: import("../playwright.ts").ResourceRef;
      at: number;
    }[];
    studioChatSwitchRecoveryBrowserFixtureConnections: {
      status: Parameters<
        Parameters<
          ReturnType<
            typeof import("../../src/servers/serverServices.ts").createServerServices
          >["resources"]["watchResourceConnection"]
        >[0]
      >[0];
      stream: Parameters<
        Parameters<
          ReturnType<
            typeof import("../../src/servers/serverServices.ts").createServerServices
          >["resources"]["watchResourceConnection"]
        >[0]
      >[1];
      at: number;
    }[];
    studioChatSwitchRecoveryBrowserFixturePanelConnections: {
      id: string;
      selected: boolean;
      connection: ReturnType<
        typeof import("../../src/hooks.ts").useMessages
      >["connection"];
      at: number;
    }[];
  }
}
