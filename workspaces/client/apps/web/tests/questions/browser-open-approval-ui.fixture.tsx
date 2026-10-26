import type { DesktopBridge } from "../../src/desktop.ts";
declare global {
  interface Window {
    browserApprovalFixture: {
      notes: string[];
      opened: Array<
        [string, Parameters<NonNullable<DesktopBridge["openSignIn"]>>[0]]
      >;
    };
  }
}
import { signInCards, browserSignInUrl } from "./sign-in.fixture.ts";
import { required } from "../messages/browser-helpers.ts";

import React from "react";
import { createRoot } from "react-dom/client";
import { MantineProvider } from "@mantine/core";
import "@mantine/core/styles.css";
import Requests from "../../src/components/questions/Requests.tsx";
const agent = {
  id: "worker",
  rootId: "lead",
  name: "Cluster worker",
  epoch: 0,
  accountKey: "default",
};
const signInCard = signInCards({
  clientId: "kube",
  scope: "openid",
  linuxUser: "studio-a-0123",
  source: "vm",
});
const requests = [
  signInCard("approve", browserSignInUrl, "accounts.example.com", {
    host: "localhost",
    port: 8000,
    path: "/",
  }),
  signInCard(
    "remote",
    "https://oidc.eu-central-1.amazonaws.com/authorize?x=1",
    "oidc.eu-central-1.amazonaws.com",
    { host: "127.0.0.1", port: 40645, path: "/oauth/callback" },
  ),
  signInCard(
    "device",
    "https://device.sso.eu-central-1.amazonaws.com/?user_code=ABCD",
    "device.sso.eu-central-1.amazonaws.com",
    null,
    { userCode: "ABCD" },
  ),
];

createRoot(required(document.getElementById("root"))).render(
  React.createElement(
    MantineProvider,
    null,
    React.createElement(Requests, {
      requests,
      allRequests: requests,
      scope: "fixture",
      agents: [agent],
      refresh: async () => {},
      notify: (s) => window.browserApprovalFixture.notes.push(s),
    }),
  ),
);
