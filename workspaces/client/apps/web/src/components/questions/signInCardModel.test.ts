import { describe, expect, it } from "vitest";
import type { BrowserOpenDetails } from "./requestData";
import {
  acceptBody,
  currentSignInWindow,
  pasteBody,
  portInUse,
  resultTarget,
  signInDeadline,
  signInErrorText,
  signInFlow,
  signInState,
  timeLeftLabel,
  verificationPage,
  waitingChatsLabel,
} from "./signInCardModel";
import { browserOpenDetails } from "./requestData";

const redirect: BrowserOpenDetails = {
  host: "accounts.example.com",
  url: "https://accounts.example.com/authorize?state=s",
  port: 8000,
  redirectPath: "/",
  source: "vm",
  linuxUser: "studio-a-1",
  expiresAt: 1000,
};
const device: BrowserOpenDetails = {
  host: "device.example.com",
  url: "https://device.example.com/activate?user_code=ABCD-EFGH",
  userCode: "ABCD-EFGH",
};

describe("sign-in card model", () => {
  it("names the flow from the redirect and the device code", () => {
    expect(signInFlow(redirect)).toBe("redirect");
    expect(signInFlow(device)).toBe("device");
    expect(
      signInFlow({ host: "x.example.com", url: "https://x.example.com/" }),
    ).toBe("page");
  });

  it("counts down to the waiting deadline once the person opened the link", () => {
    expect(signInDeadline(redirect)).toBe(1000);
    expect(
      signInDeadline({ ...redirect, phase: "waiting", waitingExpiresAt: 1500 }),
    ).toBe(1500);
    expect(timeLeftLabel(1000, 400_000)).toBe("10 min");
    expect(timeLeftLabel(1000, 959_500)).toBe("Less than 1 min");
    expect(timeLeftLabel(1000, 1_000_000)).toBe("Expired");
    expect(timeLeftLabel(undefined, 0)).toBeUndefined();
  });

  it("says where the result goes", () => {
    expect(resultTarget(redirect, "igor-mbp")).toBe(
      "The VM on igor-mbp, user studio-a-1, port 8000",
    );
    expect(resultTarget({ ...redirect, source: "host" }, undefined)).toBe(
      "the chat's server, port 8000",
    );
    expect(resultTarget(device, "igor-mbp")).toBe("No result to forward");
  });

  it("chooses the forward, a local bind, or paste mode for the approval", () => {
    expect(acceptBody({ canForward: true, loopbackPage: false })).toEqual({
      decision: "accept",
      forward: true,
    });
    expect(acceptBody({ canForward: false, loopbackPage: true })).toEqual({
      decision: "accept",
    });
    // A phone or a tab on another computer cannot hold the port: paste mode.
    expect(acceptBody({ canForward: false, loopbackPage: false })).toEqual({
      decision: "accept",
      paste: true,
    });
    expect(acceptBody({ canForward: true, loopbackPage: true }, true)).toEqual({
      decision: "accept",
      paste: true,
    });
    expect(currentSignInWindow(false, "studio.tail1234.ts.net")).toEqual({
      canForward: false,
      loopbackPage: false,
    });
    expect(currentSignInWindow(false, "127.0.0.1").loopbackPage).toBe(true);
  });

  it("sends a trimmed pasted address and nothing for an empty field", () => {
    expect(pasteBody("  http://localhost:8000/?code=c&state=s \n")).toEqual({
      decision: "answer",
      result: "http://localhost:8000/?code=c&state=s",
    });
    expect(pasteBody("   ")).toBeUndefined();
  });

  it("follows the waiting phase and a sent paste", () => {
    expect(signInState(redirect, false)).toBe("approval");
    expect(signInState({ ...redirect, phase: "waiting" }, false)).toBe(
      "waiting",
    );
    expect(signInState({ ...redirect, phase: "waiting" }, true)).toBe(
      "sending",
    );
    expect(
      signInState({ ...redirect, phase: "waiting", pasteSent: true }, false),
    ).toBe("sending");
  });

  it("finds the port refusal and the verification page", () => {
    expect(
      portInUse(
        "Port 8000 is in use on this Mac. Free it and approve again, or decline.",
      ),
    ).toBe(true);
    expect(portInUse("The sign-in command already stopped.")).toBe(false);
    expect(verificationPage(device.url)).toBe("device.example.com/activate");
    expect(verificationPage("https://d.example.com/?user_code=A")).toBe(
      "d.example.com",
    );
  });

  it.each([
    "another sign-in uses port 8000 on this computer. Wait until it ends, then approve again.",
    "another sign-in uses port 8000 on this Mac. Wait until it ends, then approve again.",
    "Port 8000 is in use on this Mac. Free it and approve again, or decline.",
  ])("offers paste mode for current and saved port conflicts: %s", (error) => {
    expect(portInUse(error)).toBe(true);
    expect(signInErrorText(error)).toBe(
      "The sign-in port is in use on this computer. Wait until the other sign-in ends, approve again, or paste the result instead.",
    );
  });

  it("preserves unrelated sign-in error details", () => {
    const error = "The sign-in command already stopped.";
    expect(portInUse(error)).toBe(false);
    expect(signInErrorText(error)).toBe(error);
  });
});

describe("chats that wait on one sign-in card", () => {
  it("reads the waiting chats of the card and names them", () => {
    const details = browserOpenDetails({
      id: "card",
      method: "browser/open",
      agent: "lead",
      status: "pending",
      params: {
        host: "id.example.com",
        url: "https://id.example.com/authorize?client_id=c&state=s",
        joined: ["worker-1", "worker-2", 7],
      },
    } as never);
    expect(details?.joined).toEqual(["worker-1", "worker-2"]);
    const names = new Map([["worker-1", "Infra"]]);
    expect(waitingChatsLabel(details?.joined, names)).toBe(
      "2 other chats wait on this sign-in: Infra and 1 more.",
    );
    expect(waitingChatsLabel(["worker-1"], names)).toBe(
      "1 other chat waits on this sign-in: Infra.",
    );
    expect(waitingChatsLabel([], names)).toBeUndefined();
    expect(waitingChatsLabel(undefined, names)).toBeUndefined();
  });
});
