import { listenHostEvent, nativeHost } from "./hostClient";
import { prepareNativeClose } from "./closeState";

type CloseRequest = { id: number; action?: "close" | "reload" };
type Closing = {
  release: () => void;
  phase: "preparing" | "responding" | "approved";
};
let owner: object | undefined;

function closeRequest(value: unknown): CloseRequest | undefined {
  if (!value || typeof value !== "object" || Array.isArray(value)) return;
  const request = value as Partial<CloseRequest>;
  if (
    typeof request.id !== "number" ||
    !Number.isSafeInteger(request.id) ||
    request.id <= 0 ||
    (request.action !== undefined &&
      request.action !== "close" &&
      request.action !== "reload")
  )
    return;
  return { id: request.id, action: request.action };
}

/** Install only in the private main document, before mounting editable views. */
export async function initializeNativeHostClose(): Promise<() => void> {
  const host = nativeHost();
  if (!host) throw new Error("The native host is unavailable.");
  if (owner) throw new Error("The native close listener is already installed.");
  const identity = {};
  owner = identity;
  const page = document;
  const stops: Array<() => void> = [];
  let disposed = false,
    listening = false,
    lastId = 0,
    closing: Closing | undefined;
  const current = () => !disposed && owner === identity;
  const alert = (id: string, text: string, dismiss = false) => {
    page.getElementById(id)?.remove();
    const message = page.createElement("div");
    message.id = id;
    message.role = "alert";
    message.style.cssText =
      "position:fixed;inset:0 0 auto;z-index:2147483647;padding:1rem;background:#7f1d1d;color:white";
    message.textContent = text;
    if (dismiss) {
      const button = page.createElement("button");
      button.textContent = "Dismiss";
      button.onclick = () => message.remove();
      message.append(button);
    }
    page.body.append(message);
  };
  const cleanup = () => {
    if (disposed) return;
    disposed = true;
    listening = false;
    // Releasing listeners cannot cancel or replace an active save operation.
    // A positive response already sent to the host keeps its uncertain outcome.
    if (closing?.phase === "preparing") closing.release();
    try {
      for (const stop of stops.splice(0).reverse()) stop();
    } finally {
      if (owner === identity) owner = undefined;
    }
  };
  const desktopError = (payload: unknown) => {
    if (
      !current() ||
      !payload ||
      typeof payload !== "object" ||
      Array.isArray(payload)
    )
      return;
    const message = (payload as { message?: unknown }).message;
    if (typeof message === "string" && message)
      alert("studio-desktop-error", message, true);
  };
  const close = async (payload: unknown) => {
    if (!current() || !listening || closing) return;
    const request = closeRequest(payload);
    if (!request) {
      alert(
        "studio-close-error",
        "The close request is invalid. The window remains open.",
      );
      return;
    }
    if (request.id <= lastId) return;
    lastId = request.id;
    const root = page.documentElement,
      previous = root.inert;
    root.inert = true;
    let released = false;
    const attempt: Closing = {
      phase: "preparing",
      release() {
        if (!released) {
          released = true;
          root.inert = previous;
        }
      },
    };
    closing = attempt;
    page.getElementById("studio-close-error")?.remove();
    try {
      await prepareNativeClose();
    } catch (error) {
      if (!current() || closing !== attempt) return;
      attempt.release();
      const message = `${request.action === "reload" ? "Reload was cancelled." : "The window is still open."} Changes may not be saved. ${String(error)}`;
      alert("studio-close-error", message);
      try {
        await host.core.invoke("close_response", {
          id: request.id,
          ready: false,
        });
      } catch (failure) {
        if (current())
          alert(
            "studio-close-error",
            `${message} The close refusal is not confirmed. ${String(failure)}`,
          );
      }
      if (current() && closing === attempt) closing = undefined;
      return;
    }
    if (!current() || closing !== attempt) return;
    attempt.phase = "responding";
    try {
      await host.core.invoke("close_response", { id: request.id, ready: true });
      if (current() && closing === attempt) attempt.phase = "approved";
    } catch (error) {
      // The close or reload may already have applied. Do not contradict it with
      // ready:false, restore editing, or repeat the positive response.
      if (current())
        alert(
          "studio-close-error",
          `The close result is not confirmed. Keep this window open. ${String(error)}`,
        );
    }
  };
  try {
    stops.push(await listenHostEvent("studio-desktop-error", desktopError));
    stops.push(await listenHostEvent("studio-close-request", close));
    listening = true;
    await host.core.invoke("close_ready", {});
    return cleanup;
  } catch (error) {
    cleanup();
    throw error;
  }
}
