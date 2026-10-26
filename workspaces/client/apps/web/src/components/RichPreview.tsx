import { memo, useEffect, useMemo, useState } from "react";
import RichPreviewView from "./RichPreviewView";
import { saveFile } from "../fileActions";
import { copyText } from "../clipboard/clipboard";

// A sandbox without permissions has an opaque origin. Remove navigation elements
// as well: sandbox alone still permits a link to navigate its own frame.
export function sanitizedDocument(source: string) {
  // This document has no browsing context. Parsing preserves html/body attributes
  // without executing scripts or loading resources before removal.
  const doc = document.implementation.createHTMLDocument("");
  doc.open();
  doc.write(source);
  doc.close();
  doc
    .querySelectorAll(
      "script, iframe, frame, frameset, object, embed, base, meta, link, animate, set, animateMotion, animateTransform",
    )
    .forEach((node) => node.remove());
  doc.querySelectorAll("*").forEach((node) => {
    for (const attr of Array.from(node.attributes)) {
      if (
        /^(src|srcset|poster|background)$/i.test(attr.name) &&
        !/^data:image\/(png|jpeg|gif|webp);base64,/i.test(attr.value)
      ) {
        node.removeAttribute(attr.name);
        continue;
      }
      if (
        /^on/i.test(attr.name) ||
        /^(href|xlink:href|action|formaction|target|srcdoc|ping|autofocus)$/i.test(
          attr.name,
        )
      ) {
        if (
          (attr.name === "href" || attr.name === "xlink:href") &&
          attr.value.startsWith("#") &&
          node.namespaceURI === "http://www.w3.org/2000/svg" &&
          [
            "use",
            "linearGradient",
            "radialGradient",
            "pattern",
            "textPath",
            "filter",
          ].includes(node.localName)
        )
          continue;
        node.removeAttribute(attr.name);
      }
    }
  });
  return doc;
}

export function isolatedDocument(
  source: string,
  options: { css?: string; dark?: boolean } = {},
) {
  const doc = sanitizedDocument(source);
  const policy = doc.createElement("meta");
  policy.httpEquiv = "Content-Security-Policy";
  policy.content =
    "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'";
  doc.head.prepend(policy);
  const style = doc.createElement("style");
  style.textContent = `html{color-scheme:${options.dark ? "dark" : "light"};background:${options.dark ? "#18191c" : "#fff"};color:${options.dark ? "#e5e5eb" : "#17202a"};font-family:system-ui,sans-serif}body{margin:16px;overflow-wrap:anywhere}img,svg{max-width:100%}*{box-sizing:border-box}`;
  policy.after(style);
  if (options.css) {
    const customStyle = doc.createElement("style");
    // HTML serialization does not escape raw style text. A CSS escape prevents
    // a closing style tag from inserting markup when srcdoc parses it again.
    customStyle.textContent = options.css.replace(/</g, "\\3c ");
    doc.head.append(customStyle);
  }
  return "<!doctype html>" + doc.documentElement.outerHTML;
}

let mermaidPromise: Promise<(typeof import("mermaid"))["default"]> | undefined;
let serial = 0;
function mermaidApi() {
  return (mermaidPromise ??= import("mermaid").then(({ default: mermaid }) => {
    mermaid.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      suppressErrorRendering: true,
      theme: "neutral",
      maxTextSize: 50000,
      maxEdges: 500,
    });
    return mermaid;
  }));
}

export default memo(function RichPreview({
  kind,
  source,
}: {
  kind: "mermaid" | "html";
  source: string;
}) {
  const [svg, setSvg] = useState("");
  const [error, setError] = useState("");
  useEffect(() => {
    if (kind !== "mermaid") return;
    let active = true;
    setSvg("");
    setError("");
    const id = `chat-diagram-${++serial}`;
    mermaidApi()
      .then((api) => api.render(id, source))
      .then((result) => {
        if (active) setSvg(result.svg);
      })
      .catch((e: unknown) => {
        if (active) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => document.getElementById(`d${id}`)?.remove());
    return () => {
      active = false;
    };
  }, [kind, source]);
  const framedDocument = useMemo(
    () => isolatedDocument(kind === "html" ? source : svg),
    [kind, source, svg],
  );
  const diagramHeight = useMemo(() => {
    if (!svg) return 160;
    // Mermaid serializes HTML labels as HTML (<br>), which is not valid XML.
    const root = new DOMParser()
      .parseFromString(svg, "text/html")
      .querySelector("svg");
    const height = Number(root?.getAttribute("viewBox")?.split(/\s+/)[3]);
    return Number.isFinite(height)
      ? Math.max(120, Math.min(560, height + 40))
      : 310;
  }, [svg]);
  return (
    <RichPreviewView
      kind={kind}
      source={source}
      error={error}
      busy={kind === "mermaid" && !svg}
      copy={copyText}
      save={(mode) => {
        const diagram = kind === "mermaid" && mode === "preview" && !!svg;
        return saveFile({
          name:
            kind === "html"
              ? "preview.html"
              : diagram
                ? "diagram.svg"
                : "diagram.mmd",
          mime: diagram
            ? "image/svg+xml"
            : kind === "html"
              ? "text/html"
              : "text/plain",
          data: diagram ? svg : source,
        });
      }}
      preview={
        <iframe
          title={
            kind === "mermaid"
              ? "Mermaid diagram preview"
              : "HTML content preview"
          }
          sandbox=""
          referrerPolicy="no-referrer"
          srcDoc={framedDocument}
          style={kind === "mermaid" ? { height: diagramHeight } : undefined}
          className={`rich-preview-frame ${kind === "mermaid" ? "rich-preview-diagram" : ""}`}
        />
      }
    />
  );
});
