import { Button, Loader } from "@mantine/core";
import { Check, Code, Copy, Download, Eye } from "lucide-react";
import { memo, useEffect, useMemo, useState } from "react";
import "./rich-preview.css";

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
  const [mode, setMode] = useState<"preview" | "source">("preview");
  const [svg, setSvg] = useState("");
  const [error, setError] = useState("");
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState("");
  useEffect(() => {
    setCopied(false);
    setCopyError("");
  }, [source]);
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
    const root = new DOMParser().parseFromString(
      svg,
      "image/svg+xml",
    ).documentElement;
    const height = Number(root.getAttribute("viewBox")?.split(/\s+/)[3]);
    return Number.isFinite(height)
      ? Math.max(120, Math.min(560, height + 40))
      : 310;
  }, [svg]);
  const [url, setUrl] = useState("");
  const downloadSvg = kind === "mermaid" && mode === "preview" && !!svg;
  useEffect(() => {
    const value = URL.createObjectURL(
      new Blob([downloadSvg ? svg : source], {
        type: downloadSvg
          ? "image/svg+xml"
          : kind === "html"
            ? "text/html"
            : "text/plain",
      }),
    );
    setUrl(value);
    return () => URL.revokeObjectURL(value);
  }, [source, svg, kind, downloadSvg]);
  return (
    <section
      className="rich-preview"
      data-preview-kind={kind}
      aria-label={kind === "mermaid" ? "Mermaid diagram" : "HTML preview"}
    >
      <div className="rich-preview-toolbar">
        <span className="rich-preview-label">
          {kind === "mermaid" ? "Mermaid" : "HTML"}
        </span>
        <div
          className="rich-preview-modes"
          role="group"
          aria-label="Preview display"
        >
          <Button
            size="compact-xs"
            variant={mode === "preview" ? "light" : "subtle"}
            aria-pressed={mode === "preview"}
            onClick={() => setMode("preview")}
            leftSection={<Eye size={13} />}
          >
            Preview
          </Button>
          <Button
            size="compact-xs"
            variant={mode === "source" ? "light" : "subtle"}
            aria-pressed={mode === "source"}
            onClick={() => setMode("source")}
            leftSection={<Code size={13} />}
          >
            Source
          </Button>
        </div>
        <Button
          size="compact-xs"
          variant="subtle"
          aria-label="Copy source"
          title="Copy source"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(source);
              setCopied(true);
              setCopyError("");
            } catch {
              setCopyError(
                "Cannot copy. Open Source and select the text to copy it.",
              );
            }
          }}
        >
          {copied ? <Check size={14} /> : <Copy size={14} />}
        </Button>
        <Button
          component="a"
          href={url}
          download={
            kind === "html"
              ? "preview.html"
              : downloadSvg
                ? "diagram.svg"
                : "diagram.mmd"
          }
          size="compact-xs"
          variant="subtle"
          aria-label="Download preview"
          title="Download preview"
        >
          <Download size={14} />
        </Button>
      </div>
      {copyError && (
        <p role="alert" className="rich-preview-error">
          {copyError}
        </p>
      )}
      {mode === "source" ? (
        <pre className="rich-preview-source">
          <code>{source}</code>
        </pre>
      ) : error ? (
        <div className="rich-preview-error" role="alert">
          <strong>
            {kind === "mermaid"
              ? "Cannot render this diagram"
              : "Preview error"}
          </strong>
          <pre>{error}</pre>
          <p>Open Source to inspect the original text.</p>
        </div>
      ) : kind === "mermaid" && !svg ? (
        <div className="rich-preview-loading">
          <Loader size="sm" />
          Render diagram
        </div>
      ) : (
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
      )}
      <div className="rich-preview-note">
        {kind === "html"
          ? "Static preview. Scripts and external resources are disabled."
          : "Diagram preview. Source remains available."}
      </div>
    </section>
  );
});
