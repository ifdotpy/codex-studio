import {
  API_SCHEMA_HASH,
  API_SCHEMA_HASH_HEADER,
} from "../generated/apiSchema";

// Transport to the client core that the server of this page hosts for the
// browser device (docs/desktop/hosted-client-core.md). The device identity is
// an HttpOnly cookie that only the server reads.
const REQUEST_PATH = "/api/client/request";
const EVENTS_PATH = "/api/client/events";

/** The command may or may not have run. The page must not send it again by itself. */
export class OutcomeUnknownError extends Error {
  constructor(cause: unknown) {
    super(
      "The connection to the server was lost. The command outcome is unknown.",
      { cause },
    );
    this.name = "OutcomeUnknownError";
  }
}

export function hasHostedClient() {
  return (
    /^https?:$/.test(globalThis.location?.protocol ?? "") &&
    typeof globalThis.fetch === "function" &&
    typeof globalThis.EventSource === "function"
  );
}

let token: Promise<string> | undefined;
let contact: Promise<void> | undefined;
let source: EventSource | undefined;
const listeners = new Set<(value: unknown) => void>();

function sessionToken(): Promise<string> {
  token ??= fetch("/api/session", { credentials: "same-origin" })
    .then(async (response) => {
      const value = (await response.json()) as { token?: unknown };
      if (!response.ok || typeof value?.token !== "string" || !value.token)
        throw new Error("The server session is unavailable.");
      return value.token;
    })
    .catch((error: unknown) => {
      token = undefined;
      throw error;
    });
  return token;
}

/**
 * The first contact gets the device cookie. Later contacts wait for it, so
 * one page never makes two devices with concurrent first requests.
 */
async function afterContact<T>(work: () => Promise<T>): Promise<T> {
  if (!contact) {
    const first = work();
    contact = first.then(
      () => undefined,
      () => {
        contact = undefined;
      },
    );
    return first;
  }
  await contact;
  return work();
}

/** Sends one command once. It returns the raw core reply for its identity. */
export async function hostedRequest(
  id: string,
  method: string,
  params: unknown,
): Promise<unknown> {
  const authorization = await sessionToken();
  let response: Response;
  try {
    response = await afterContact(() =>
      fetch(REQUEST_PATH, {
        method: "POST",
        credentials: "same-origin",
        headers: {
          "content-type": "application/json",
          "x-canvas-token": authorization,
          [API_SCHEMA_HASH_HEADER]: API_SCHEMA_HASH,
        },
        body: JSON.stringify({ id, method, params }),
      }),
    );
  } catch (error) {
    throw new OutcomeUnknownError(error);
  }
  let value: unknown;
  try {
    value = await response.json();
  } catch (error) {
    throw new OutcomeUnknownError(error);
  }
  if (!response.ok) {
    // A refused token is read again for the next command; this one is not repeated.
    if (response.status === 403) token = undefined;
    const message = (value as { error?: unknown } | null)?.error;
    throw new Error(
      typeof message === "string" && message
        ? message
        : `The server refused the command (${response.status}).`,
    );
  }
  return value;
}

function openEvents(): Promise<void> {
  if (source) return Promise.resolve();
  const events = new EventSource(EVENTS_PATH);
  source = events;
  events.onmessage = (event: MessageEvent<string>) => {
    let value: unknown;
    try {
      value = JSON.parse(event.data);
    } catch {
      return;
    }
    for (const listener of listeners) listener(value);
  };
  return new Promise((resolve, reject) => {
    events.addEventListener("open", () => resolve(), { once: true });
    events.addEventListener(
      "error",
      () => reject(new Error("The server event stream is unavailable.")),
      { once: true },
    );
  });
}

/** All listeners of the page share one event stream. */
export async function listenHostedEvents(
  callback: (value: unknown) => void,
): Promise<() => void> {
  listeners.add(callback);
  const unlisten = () => {
    listeners.delete(callback);
    if (listeners.size || !source) return;
    source.close();
    source = undefined;
  };
  // The browser reopens a failed stream by itself; a failed first open only
  // lets the next contact get the device cookie.
  if (!source) await afterContact(openEvents).catch(() => undefined);
  return unlisten;
}
