declare global {
  interface Window {
    signInPhoneFixture: {
      notes: string[];
      opened: string[];
      copied: string[];
      waiting: () => void;
    };
  }
}
import { signInCards, phoneSignInUrl } from "./sign-in.fixture.ts";
import { required } from "../messages/browser-helpers.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import "../../src/studio-theme.css";
import Requests from "../../src/components/questions/Requests.tsx";
const agent = {
  id: "worker",
  rootId: "lead",
  name: "Cluster worker with a long chat name",
  epoch: 0,
  accountKey: "default",
};
const now = Date.now() / 1000;
const signInCard = signInCards({
  clientId: "cli-0123456789abcdef",
  scope: "sso:account:access openid profile",
  source: "host",
  expiresAt: now + 540,
});
let requests = [
  signInCard("redirect", phoneSignInUrl, "id.example.com", {
    host: "127.0.0.1",
    port: 40645,
    path: "/oauth/callback",
  }),
  signInCard(
    "device",
    "https://device.sso.eu-central-1.amazonaws.com/?user_code=WDJB-MJHT",
    "device.sso.eu-central-1.amazonaws.com",
    null,
    { userCode: "WDJB-MJHT" },
  ),
];

const rootNode = createRoot(required(document.getElementById("root")));
const render = () =>
  rootNode.render(
    React.createElement(
      MantineProvider,
      null,
      React.createElement(
        "div",
        { style: { padding: "0 16px" } },
        React.createElement(Requests, {
          requests,
          allRequests: requests,
          scope: "fixture",
          agents: [agent],
          refresh: async () => {},
          notify: (s) => window.signInPhoneFixture.notes.push(s),
        }),
      ),
    ),
  );
// The server keeps the card pending in the waiting phase after a paste-mode approval.
window.signInPhoneFixture.waiting = () => {
  requests = requests.map((r) =>
    r.id === "redirect"
      ? {
          ...r,
          params: {
            ...r.params,
            phase: "waiting",
            waiting: { expiresAt: now + 600 },
          },
        }
      : r,
  );
  render();
};
render();
