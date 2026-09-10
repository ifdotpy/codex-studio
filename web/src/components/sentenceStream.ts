import { marked } from "marked";

const segmenter = new Intl.Segmenter(["ru", "en"], { granularity: "sentence" });
export function sentences(text: string) {
  return Array.from(segmenter.segment(text), ({ segment, index }) => ({
    text: segment,
    index,
  }));
}

// Markdown blocks remain intact. Within prose, release each finished sentence.
export function sentencePrefix(text: string, streaming: boolean) {
  if (!streaming) return text;
  let end = 0,
    offset = 0,
    fence = "";
  for (const line of text.match(/[^\n]*\n|[^\n]+$/g) || []) {
    offset += line.length;
    const marker = line.match(/^ {0,3}(`{3,}|~{3,})/);
    if (marker) {
      if (!fence) fence = marker[1];
      else if (
        marker[1][0] === fence[0] &&
        marker[1].length >= fence.length &&
        /^ {0,3}(`{3,}|~{3,})\s*$/.test(line)
      ) {
        fence = "";
        end = offset;
      }
    } else if (!fence && /^\s*\n$/.test(line)) end = offset;
  }
  const tail = text.slice(end);
  const tokens = marked.lexer(tail);
  let cursor = end;
  for (const token of tokens) {
    if (token.type === "space") {
      cursor += token.raw.length;
      end = cursor;
      continue;
    }
    if (token.type === "code" || token.type === "html") break;
    if (token.type !== "paragraph") {
      // Lists, tables and headings need a complete line to retain their structure.
      const lineEnd = token.raw.lastIndexOf("\n");
      if (lineEnd >= 0) end = cursor + lineEnd + 1;
      break;
    }
    let inlineOffset = 0;
    const ranges = (token.tokens || []).map((inline) => {
      const start = inlineOffset;
      inlineOffset += inline.raw.length;
      return { type: inline.type, raw: inline.raw, start, end: inlineOffset };
    });
    const ends = new Set(
      sentences(token.raw).map((part) => part.index + part.text.length),
    );
    // ICU does not treat Markdown closing delimiters as sentence punctuation.
    for (const match of token.raw.matchAll(/[.!?…。！？]["'»”’)*_\]]+\s+/g))
      ends.add(match.index + match[0].length);
    for (const candidate of [...ends].sort((a, b) => a - b)) {
      const prefix = token.raw.slice(0, candidate);
      if (!/[.!?…。！？]["'»”’)*_\]]*\s*$/.test(prefix)) continue;
      // A period at the chunk edge can still become a decimal or a URL.
      if (
        candidate === token.raw.length &&
        !/\s$/.test(prefix) &&
        /[.]["'»”’)*_\]]*$/.test(prefix)
      )
        continue;
      // Keep Markdown inline constructs intact at a sentence boundary.
      if (
        ranges.some(
          (inline) =>
            ["link", "image", "codespan", "strong", "em", "del"].includes(
              inline.type,
            ) &&
            inline.start < candidate &&
            candidate < inline.end,
        )
      )
        continue;
      if (
        ranges.some(
          (inline) =>
            inline.type === "text" &&
            inline.start < candidate &&
            /(?<!\\)(`|\[|\*\*|__|~~)/.test(
              inline.raw.slice(0, candidate - inline.start),
            ),
        )
      )
        continue;
      end = cursor + candidate;
    }
    break;
  }
  return text.slice(0, end);
}
