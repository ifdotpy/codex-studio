import type { Json } from "../../types";
import type { BrowserOpenDetails } from "./requestData";

/** The kind of sign-in that a `browser/open` card asks for (docs/vm-browser-bridge.md, 6.2). */
export type SignInFlow = "redirect" | "device" | "page";

export function signInFlow(details: BrowserOpenDetails): SignInFlow {
  if (details.port !== undefined) return "redirect";
  return details.userCode ? "device" : "page";
}

export const SIGN_IN_FLOW_LABEL: Record<SignInFlow, string> = {
  redirect: "Browser sign-in",
  device: "Device code",
  page: "Sign-in page",
};

/** The deadline that the card counts down to, in epoch seconds. */
export function signInDeadline(details: BrowserOpenDetails) {
  return details.phase === "waiting"
    ? (details.waitingExpiresAt ?? details.expiresAt)
    : details.expiresAt;
}

/** "9 min", "Less than 1 min" or "Expired"; undefined without a deadline. */
export function timeLeftLabel(
  deadlineSeconds: number | undefined,
  nowMs: number,
): string | undefined {
  if (deadlineSeconds === undefined) return undefined;
  const seconds = deadlineSeconds - nowMs / 1000;
  if (seconds <= 0) return "Expired";
  if (seconds < 60) return "Less than 1 min";
  return `${Math.floor(seconds / 60)} min`;
}

/** Where the sign-in result goes, in words for the card. */
export function resultTarget(
  details: BrowserOpenDetails,
  serverName: string | undefined,
): string {
  if (details.port === undefined) return "No result to forward";
  const server = serverName || "the chat's server";
  if (details.source === "vm")
    return `The Linux VM on ${server}${details.linuxUser ? `, user ${details.linuxUser}` : ""}, port ${details.port}`;
  return `${server}, port ${details.port}`;
}

/** What the window can do with the sign-in redirect after the approval. */
export type SignInWindow = {
  /** The desktop (Electron or Tauri) can hold the redirect port on this computer. */
  canForward: boolean;
  /** The window is a browser tab on the computer that runs the chat's server. */
  loopbackPage: boolean;
};

/** The accept body for "Open in browser". A desktop asks for the forward; the chat's server
 * ignores it for a window on its own Mac. A browser tab on that Mac lets the server bind the
 * port. Every other window (a phone, a tab on another computer) uses paste mode. */
export function acceptBody(window: SignInWindow, paste = false): Json {
  if (paste) return { decision: "accept", paste: true };
  if (window.canForward) return { decision: "accept", forward: true };
  if (window.loopbackPage) return { decision: "accept" };
  return { decision: "accept", paste: true };
}

export function currentSignInWindow(
  canForward: boolean,
  hostname = globalThis.location?.hostname ?? "",
): SignInWindow {
  return {
    canForward,
    loopbackPage: ["localhost", "127.0.0.1", "[::1]", "::1"].includes(hostname),
  };
}

/** The body that sends a pasted address to the command, or undefined for an empty text. */
export function pasteBody(text: string): Json | undefined {
  const result = text.trim();
  return result ? { decision: "answer", result } : undefined;
}

/** The server refused the forward because a program on this Mac holds the port. */
export const portInUse = (error: string) =>
  error.includes("is in use on this Mac");

/** The page that a device code is typed into: the link without its query. */
export function verificationPage(url: string): string {
  try {
    const parsed = new URL(url);
    return parsed.host + (parsed.pathname === "/" ? "" : parsed.pathname);
  } catch {
    return url;
  }
}

export type SignInState = "approval" | "waiting" | "sending";

export function signInState(
  details: BrowserOpenDetails,
  sending: boolean,
): SignInState {
  if (details.phase !== "waiting") return "approval";
  return sending || details.pasteSent ? "sending" : "waiting";
}

export const SIGN_IN_STATE_LABEL: Record<SignInState, string> = {
  approval: "Waiting for you",
  waiting: "Waiting for the browser",
  sending: "Sending the result",
};

/** The answer of a pasted result does not name the outcome; the chat event does (6.2). */
export const RESULT_SENT_NOTE =
  "Studio sent the sign-in result to the command. The chat shows how it ended.";

/**
 * The chats that wait on this card, in words: "2 other chats wait on this sign-in: Infra and
 * 1 more." Undefined when no other chat waits. `names` maps agent IDs to chat names.
 */
export function waitingChatsLabel(
  joined: readonly string[] | undefined,
  names: ReadonlyMap<string, string>,
): string | undefined {
  if (!joined?.length) return undefined;
  const known = joined
    .map((agent) => names.get(agent))
    .filter((name): name is string => !!name);
  const count = joined.length;
  const lead =
    count === 1
      ? "1 other chat waits on this sign-in"
      : `${count} other chats wait on this sign-in`;
  if (!known.length) return `${lead}.`;
  const rest = count - known.length;
  return `${lead}: ${known.join(", ")}${rest ? ` and ${rest} more` : ""}.`;
}
