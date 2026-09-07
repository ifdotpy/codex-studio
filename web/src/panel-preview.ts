import "./studio-theme.css";
import {
  panelContentDocument,
  type PanelContent,
} from "./components/PanelDocument";

type PreviewPanel = PanelContent & {
  submittedCallbacks?: string[];
};

declare global {
  interface Window {
    renderPanelPreview: (panel: PreviewPanel) => Promise<void>;
  }
}

// This entry has no workspace, API client, native bridge, or callback receiver.
// It renders an immutable tool argument in the same document as the visible panel.
window.renderPanelPreview = (panel) =>
  new Promise<void>((resolve, reject) => {
    const frame = document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-scripts allow-forms");
    frame.setAttribute("title", "Agent panel");
    const channel = crypto.randomUUID();
    const timeout = setTimeout(() => {
      removeEventListener("message", ready);
      reject(new Error("The panel document did not finish rendering."));
    }, 5000);
    function ready(event: MessageEvent) {
      if (
        event.source !== frame.contentWindow ||
        event.data?.channel !== channel ||
        !["panel-ready", "panel-error"].includes(event.data?.type)
      )
        return;
      removeEventListener("message", ready);
      clearTimeout(timeout);
      if (event.data.type === "panel-error") {
        reject(new Error(event.data.error || "Invalid panel spec"));
        return;
      }
      // Match the available controls in the user panel. There is no callback
      // receiver here, so the screenshot cannot send an action to an agent.
      frame.contentWindow?.postMessage(
        {
          type: "panel-state",
          channel,
          busy: false,
          locked: panel.submittedCallbacks || [],
        },
        "*",
      );
      requestAnimationFrame(() => requestAnimationFrame(() => resolve()));
    }
    addEventListener("message", ready);
    frame.srcdoc = panelContentDocument(panel, channel);
    document.body.replaceChildren(frame);
  });
