import { marked } from "marked";
import type { Message } from "../types";
import { localFileLink } from "./fileLinks";

interface ResultBase {
  id: string;
  messageId: string;
  label: string;
}
export type ConversationResult = ResultBase &
  (
    | { kind: "file"; path: string; line?: number; image: boolean }
    | { kind: "asset"; asset: string; image: boolean }
    | {
        kind: "patch";
        source: string;
        paths: string[];
        truncated: boolean;
        change?: unknown;
      }
    | { kind: "html" | "mermaid"; source: string }
  );

const filename = (path: string) => path.split("/").at(-1) || path;
const plainLabel = (text: string) =>
  text
    .replace(/<[^>]*>/g, "")
    .replace(/[*`]/g, "")
    .trim();
const imagePath = (path: string) => /\.(png|jpe?g|gif|webp|svg)$/i.test(path);

function patchPaths(source: string) {
  const paths = new Set<string>();
  for (const match of source.matchAll(/^(?:---|\+\+\+) (.+)$/gm)) {
    let path = match[1].split("\t")[0];
    if (path.startsWith('"')) {
      try {
        path = JSON.parse(path);
      } catch {
        continue;
      }
    }
    if (path !== "/dev/null") paths.add(path.replace(/^[ab]\//, ""));
  }
  return [...paths];
}

// The caller supplies the available history. No reads, path guesses, or server
// requests belong here. A patch is the saved payload, never today's git diff.
export function conversationResults(messages: Message[]): ConversationResult[] {
  const results: ConversationResult[] = [];
  const seen = new Set<string>();
  const add = (result: ConversationResult, identity: string) => {
    if (seen.has(identity)) return;
    seen.add(identity);
    results.push(result);
  };
  for (const message of messages) {
    if (message.pending || message.streaming) continue;
    let serial = 0;
    const base = (label: string) => ({
      id: `${message.id}:result:${serial++}`,
      messageId: message.id,
      label,
    });
    if (message.role === "assistant") {
      const tokens = marked.lexer(message.text || "");
      marked.walkTokens(tokens, (token) => {
        if (token.type === "link" || token.type === "image") {
          try {
            const target = localFileLink(token.href);
            if (!target) return;
            const label = plainLabel(token.text) || filename(target.path);
            add(
              {
                ...base(label),
                kind: "file",
                ...target,
                image: token.type === "image" || imagePath(target.path),
              },
              `file:${target.path.replace(/^\.\//, "")}`,
            );
          } catch {
            // Invalid or remote file links stay in the original message.
          }
        }
        if (token.type !== "code") return;
        const language = token.lang?.trim().toLowerCase();
        if (language !== "html" && language !== "mermaid") return;
        const lines = token.raw.trimEnd().split("\n");
        const open = lines[0].match(/^ {0,3}(`{3,}|~{3,})/);
        const close = lines.at(-1)?.match(/^ {0,3}(`{3,}|~{3,})\s*$/);
        if (
          lines.length < 2 ||
          !open ||
          !close ||
          open[1][0] !== close[1][0] ||
          close[1].length < open[1].length
        )
          return;
        add(
          {
            ...base(language === "html" ? "HTML preview" : "Mermaid diagram"),
            kind: language,
            source: token.text,
          },
          `${language}:${token.text}`,
        );
      });
      // Match the existing Markdown renderer's adjacent HTML block grouping.
      let html = "";
      const flush = () => {
        if (html.trim())
          add(
            { ...base("HTML preview"), kind: "html", source: html },
            `html:${html}`,
          );
        html = "";
      };
      for (const token of tokens) {
        if (token.type === "html") html += (html ? "\n" : "") + token.text;
        else if (token.type !== "space") flush();
      }
      flush();
      if (Array.isArray(message.assets)) {
        for (const asset of message.assets) {
          if (!asset || typeof asset.id !== "string" || !asset.id) continue;
          add(
            {
              ...base(
                typeof asset.name === "string" ? asset.name : "Attachment",
              ),
              kind: "asset",
              asset: asset.id,
              image: asset.image === true,
            },
            `asset:${asset.id}`,
          );
        }
      }
    }
    if (
      message.role !== "output" ||
      ["running", "failed", "declined", "cancelled", "interrupted"].includes(
        message.toolStatus,
      )
    )
      continue;
    let payload;
    try {
      payload = JSON.parse(message.text);
    } catch {
      continue;
    }
    if (!payload || typeof payload !== "object") continue;
    if (payload.success === false || payload.error) continue;
    if (["inProgress", "failed", "declined"].includes(payload.status)) continue;
    if (payload.type === "fileChange" && Array.isArray(payload.changes)) {
      if (payload.status !== "completed" && message.toolStatus !== "completed")
        continue;
      for (const change of payload.changes) {
        if (
          !change ||
          typeof change.path !== "string" ||
          typeof change.diff !== "string" ||
          !change.diff.trim()
        )
          continue;
        add(
          {
            ...base(filename(change.path)),
            kind: "patch",
            source: change.diff,
            change,
            paths: [change.path],
            truncated: !!message.truncated,
          },
          `patch:${change.path}:${change.diff}`,
        );
      }
    } else if (
      (message.title === "Changes" ||
        message.title === "turn/diff/updated" ||
        message.id.endsWith("turn/diff/updated")) &&
      typeof payload.diff === "string" &&
      payload.diff.trim()
    ) {
      const paths = patchPaths(payload.diff);
      add(
        {
          ...base(
            paths.length === 1
              ? filename(paths[0])
              : `Changes${paths.length ? ` · ${paths.length} files` : ""}`,
          ),
          kind: "patch",
          source: payload.diff,
          paths,
          truncated: !!message.truncated,
        },
        `patch:${paths.length === 1 ? paths[0] : ""}:${payload.diff}`,
      );
    }
  }
  return results;
}
