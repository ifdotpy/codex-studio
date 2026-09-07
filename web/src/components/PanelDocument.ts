import studioTheme from "../studio-theme.css?inline";
import { sanitizedDocument } from "./RichPreview";

export interface PanelCallback {
  id: string;
  label: string;
  fields?: string[];
}

function preparePanelFrame(
  doc: Document,
  nonce: string,
  channel: string,
  layoutOnly = false,
) {
  const background =
    getComputedStyle(document.documentElement)
      .getPropertyValue("--surface")
      .trim() || "#1b1b20";
  for (const root of [doc.documentElement, doc.body]) {
    root.style.setProperty("background", background, "important");
    root.style.setProperty("color-scheme", "dark");
  }
  const script = doc.createElement("script");
  script.setAttribute("nonce", nonce);
  if (!layoutOnly) return;
  script.src = new URL("assets/panel-bridge.js", document.baseURI).href;
  script.setAttribute(
    "data-config",
    encodeURIComponent(JSON.stringify({ channel, layoutOnly: true })),
  );
  doc.body.append(script);
}

export function panelDocument(
  source: string,
  css: string,
  callbacks: PanelCallback[],
  channel: string,
) {
  const doc = sanitizedDocument(source);
  const nonce = crypto.randomUUID().replaceAll("-", "");
  const policy = doc.createElement("meta");
  policy.httpEquiv = "Content-Security-Policy";
  policy.content = `default-src 'none'; script-src 'nonce-${nonce}'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'`;
  doc.head.prepend(policy);
  const style = doc.createElement("style");
  style.textContent =
    studioTheme +
    `:root{color-scheme:dark;--studio-bg:var(--surface);--studio-surface:#232329;--studio-text:#e9e9ee;--studio-muted:#9696a5;--studio-accent:#a399ff;--studio-border:#3a3a43;--studio-radius:8px;--studio-font:Inter,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}*{box-sizing:border-box}html{height:100%;background:var(--studio-bg);color:var(--studio-text);font:13px/1.45 var(--studio-font)}body{height:100%;margin:0;overflow-wrap:anywhere}h1,h2,h3,h4,p{margin:0 0 8px}h1,h2,h3{font-size:14px;font-weight:600}small,summary{color:var(--studio-muted)}button,input,select,textarea{font:inherit;color:inherit}button,input[type=submit],input[type=button]{border:1px solid var(--studio-border);border-radius:var(--studio-radius);background:var(--studio-surface);padding:5px 10px;cursor:pointer}button:hover{border-color:var(--studio-accent)}:focus-visible{outline:2px solid var(--studio-accent);outline-offset:2px}:disabled{opacity:.45;cursor:default}input:not([type=checkbox]):not([type=radio]),select,textarea{max-width:100%;border:1px solid var(--studio-border);border-radius:var(--studio-radius);background:var(--studio-surface);padding:5px 8px}input[type=checkbox],input[type=radio],progress{accent-color:var(--studio-accent)}label{font-size:12px}fieldset{border:1px solid var(--studio-border);border-radius:var(--studio-radius)}progress{max-width:100%;height:6px}hr{border:0;border-top:1px solid var(--studio-border)}img,svg{max-width:100%}code,pre{font-family:SFMono-Regular,Consolas,monospace}table{border-collapse:collapse}td,th{padding:4px 8px;text-align:left;border-bottom:1px solid var(--studio-border)}`;
  policy.after(style);
  if (css) {
    const custom = doc.createElement("style");
    custom.textContent = css.replace(/</g, "\\3c ");
    doc.head.append(custom);
  }
  // The native preflight checks the full geometry, including clipped content.
  // These host rules remove scrolling; they do not turn overflow into a valid fit.
  const bounds = doc.createElement("style");
  bounds.textContent =
    "html,body{overflow:hidden!important;overscroll-behavior:none!important}*{scrollbar-width:none!important}";
  doc.head.append(bounds);
  const canvasBackground = getComputedStyle(document.documentElement)
    .getPropertyValue("--surface")
    .trim();
  for (const root of [doc.documentElement, doc.body]) {
    if (canvasBackground)
      root.style.setProperty("background", canvasBackground, "important");
    root.style.setProperty("overflow", "hidden", "important");
    root.style.setProperty("overscroll-behavior", "none", "important");
  }
  preparePanelFrame(doc, nonce, channel);
  const script = doc.createElement("script");
  script.setAttribute("nonce", nonce);
  script.src = new URL("assets/panel-bridge.js", document.baseURI).href;
  script.setAttribute(
    "data-config",
    encodeURIComponent(JSON.stringify({ channel, callbacks })),
  );
  doc.body.append(script);
  return "<!doctype html>" + doc.documentElement.outerHTML;
}

export interface PanelContent {
  dataVersion?: number;
  feed?: { statePath: string } | null;
  html: string;
  css: string;
  format?: string;
  spec?: unknown;
  callbacks?: PanelCallback[];
}

export function panelContentDocument(panel: PanelContent, channel: string) {
  if (panel.format !== "json-render")
    return panelDocument(panel.html, panel.css, panel.callbacks || [], channel);
  const doc = document.implementation.createHTMLDocument("");
  const nonce = crypto.randomUUID().replaceAll("-", "");
  const policy = doc.createElement("meta");
  policy.httpEquiv = "Content-Security-Policy";
  policy.content = `default-src 'none'; script-src 'nonce-${nonce}'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'`;
  doc.head.prepend(policy);
  const root = doc.createElement("div");
  root.id = "panel-root";
  doc.body.append(root);
  preparePanelFrame(doc, nonce, channel, true);
  const script = doc.createElement("script");
  script.setAttribute("nonce", nonce);
  script.src = new URL("assets/panel-ui.js", document.baseURI).href;
  script.setAttribute(
    "data-config",
    encodeURIComponent(
      JSON.stringify({
        channel,
        dataVersion: panel.dataVersion || 0,
        feed: panel.feed,
        spec: panel.spec,
        callbacks: panel.callbacks || [],
        background: getComputedStyle(document.documentElement)
          .getPropertyValue("--surface")
          .trim(),
      }),
    ),
  );
  doc.body.append(script);
  return "<!doctype html>" + doc.documentElement.outerHTML;
}
