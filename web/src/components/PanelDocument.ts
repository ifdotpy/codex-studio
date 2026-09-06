import { sanitizedDocument } from "./RichPreview";

export interface PanelCallback {
  id: string;
  label: string;
  fields?: string[];
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
  style.textContent = `:root{color-scheme:dark;--studio-bg:#18191c;--studio-surface:#232329;--studio-text:#e9e9ee;--studio-muted:#9696a5;--studio-accent:#a399ff;--studio-border:#3a3a43;--studio-radius:8px;--studio-font:Inter,-apple-system,BlinkMacSystemFont,"Segoe UI",sans-serif}*{box-sizing:border-box}html{background:var(--studio-bg);color:var(--studio-text);font:13px/1.45 var(--studio-font)}body{margin:12px 16px;overflow-wrap:anywhere}h1,h2,h3,h4,p{margin:0 0 8px}h1,h2,h3{font-size:14px;font-weight:600}small,summary{color:var(--studio-muted)}button,input,select,textarea{font:inherit;color:inherit}button,input[type=submit],input[type=button]{border:1px solid var(--studio-border);border-radius:var(--studio-radius);background:var(--studio-surface);padding:5px 10px;cursor:pointer}button:hover{border-color:var(--studio-accent)}:focus-visible{outline:2px solid var(--studio-accent);outline-offset:2px}:disabled{opacity:.45;cursor:default}input:not([type=checkbox]):not([type=radio]),select,textarea{max-width:100%;border:1px solid var(--studio-border);border-radius:var(--studio-radius);background:var(--studio-surface);padding:5px 8px}input[type=checkbox],input[type=radio],progress{accent-color:var(--studio-accent)}label{font-size:12px}fieldset{border:1px solid var(--studio-border);border-radius:var(--studio-radius)}progress{max-width:100%;height:6px}hr{border:0;border-top:1px solid var(--studio-border)}img,svg{max-width:100%}code,pre{font-family:SFMono-Regular,Consolas,monospace}table{border-collapse:collapse}td,th{padding:4px 8px;text-align:left;border-bottom:1px solid var(--studio-border)}`;
  policy.after(style);
  if (css) {
    const custom = doc.createElement("style");
    custom.textContent = css.replace(/</g, "\\3c ");
    doc.head.append(custom);
  }
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
