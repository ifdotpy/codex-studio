import DOMPurify from "dompurify";
import type { LocalFileLink } from "./fileLinks";

export function relativeToDocument(
  target: LocalFileLink,
  basePath?: string,
): LocalFileLink {
  if (!basePath || target.path.startsWith("/")) return target;
  const slash = basePath.lastIndexOf("/");
  return slash < 0
    ? target
    : { ...target, path: basePath.slice(0, slash + 1) + target.path };
}

export function fileFormat(name: string, mime: string) {
  const ext = name.split(".").at(-1)?.toLowerCase();
  if (mime === "image/svg+xml" || ext === "svg") return "svg";
  if (/^image\/(png|jpeg|gif|webp|avif|bmp|x-icon)$/.test(mime)) return "image";
  if (mime.startsWith("audio/")) return "audio";
  if (mime.startsWith("video/")) return "video";
  if (mime === "application/pdf") return "pdf";
  if (mime === "text/html" || /^(html|htm)$/.test(ext || "")) return "html";
  if (mime === "text/markdown" || /^(md|markdown)$/.test(ext || ""))
    return "markdown";
  if (ext === "csv" || mime === "text/csv") return "csv";
  if (ext === "tsv" || mime === "text/tab-separated-values") return "tsv";
  if (/^application\/(?:[\w.-]+\+)?json$/.test(mime) || ext === "json")
    return "json";
  if (
    /^(text\/|application\/(xml|javascript))/.test(mime) ||
    /^(tsx?|jsx?|py|rs|toml|yaml|yml|log|diff|sh|ini|conf)$/.test(ext || "")
  )
    return "text";
  return "unknown";
}

// Use this output exclusively as an img source, never inline HTML or a document.
// SVG image context disables scripts and external resources. Preserve CSS and
// local paint references; DOMPurify removes executable markup as defense in depth.
export function safeSvg(source: string): string {
  const clean = DOMPurify.sanitize(source, {
    USE_PROFILES: { svg: true, svgFilters: true },
    FORBID_TAGS: [
      "script",
      "foreignObject",
      "animate",
      "animateMotion",
      "animateTransform",
      "set",
    ],
  });
  const doc = new DOMParser().parseFromString(clean, "image/svg+xml");
  if (
    doc.querySelector("parsererror") ||
    doc.documentElement.localName !== "svg"
  )
    throw new Error("This SVG file is invalid.");
  return new XMLSerializer().serializeToString(doc.documentElement);
}

export function parseDelimited(
  source: string,
  delimiter: string,
  maxRows = 200,
  maxColumns = 100,
  maxChars = 1024 * 1024,
) {
  const rows: string[][] = [];
  let row: string[] = [],
    field = "",
    quoted = false,
    closed = false,
    truncated = false;
  const end = Math.min(source.length, maxChars);
  for (let i = 0; i < end; i++) {
    const c = source[i];
    if (quoted) {
      if (c === '"') {
        if (source[i + 1] === '"') {
          field += '"';
          i++;
        } else {
          quoted = false;
          closed = true;
        }
      } else field += c;
      continue;
    }
    if (c === '"' && !field && !closed) {
      quoted = true;
      continue;
    }
    if (c === delimiter || c === "\n" || c === "\r") {
      row.push(field);
      field = "";
      closed = false;
      if (
        row.length > maxColumns ||
        (c === delimiter && row.length >= maxColumns)
      ) {
        truncated = true;
        break;
      }
      if (c !== delimiter) {
        rows.push(row);
        row = [];
        if (c === "\r" && source[i + 1] === "\n") i++;
        if (rows.length >= maxRows && i + 1 < source.length) {
          truncated = true;
          break;
        }
      }
    } else {
      if (closed || c === '"')
        return {
          rows,
          truncated: false,
          error: "Invalid quoted field. Use Source to read the original file.",
        };
      field += c;
    }
  }
  truncated ||= end < source.length;
  if (!truncated && quoted)
    return {
      rows,
      truncated: false,
      error: "Unclosed quoted field. Use Source to read the original file.",
    };
  if (!truncated && (field || closed || row.length)) {
    row.push(field);
    rows.push(row);
  }
  return { rows, truncated, error: "" };
}
