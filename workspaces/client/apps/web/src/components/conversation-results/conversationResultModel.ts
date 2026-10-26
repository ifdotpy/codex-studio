import { marked } from "marked";
import type { Message } from "../../types";
import { localFileLink } from "../fileLinks";

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
  for (const match of source.matchAll(/^(?:---|\+\+\+) (.+)/gm)) {
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
      // Stryker disable next-line StringLiteral: A plain fallback produces no result; the empty value also keeps missing text from crashing Marked.
      const tokens = marked.lexer(message.text || "");
      marked.walkTokens(tokens, (token) => {
        switch (token.type) {
          case "link":
          case "image": {
            let target;
            try {
              target = localFileLink(token.href);
            } catch {
              // The undefined target is rejected by the guard below.
            }
            if (!target) break;
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
            break;
          }
          case "code": {
            const language = token.lang?.trim().toLowerCase();
            if (language !== "html" && language !== "mermaid") break;
            const lines = token.raw.trimEnd().split("\n");
            // Stryker disable next-line Regex: Only the removed-start-anchor variant survives because Marked emits code tokens only for recognized fence-start lines; indentation and fence-length variants are killed by “rich output retains exact source, merges raw HTML, and excludes unfinished fences” and “only closed supported fences become previews”.
            const open = lines[0].match(/^ {0,3}(`{3,}|~{3,})/);
            const close = lines.at(-1)!.match(/^ {0,3}(`{3,}|~{3,})\s*$/);
            if (
              !open ||
              !close ||
              open[1][0] !== close[1][0] ||
              close[1].length < open[1].length
            )
              break;
            add(
              {
                ...base(
                  language === "html" ? "HTML preview" : "Mermaid diagram",
                ),
                kind: language,
                source: token.text,
              },
              `${language}:${token.text}`,
            );
            break;
          }
        }
      });
      // Match the existing Markdown renderer's adjacent HTML block grouping.
      let html = "";
      const flush = () => {
        if (html)
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
        message.toolStatus ?? "",
      )
    )
      continue;
    let payload: unknown;
    try {
      payload = JSON.parse(message.text) as unknown;
    } catch {
      // The following falsy-payload check skips malformed JSON.
    }
    if (!payload || typeof payload !== "object" || Array.isArray(payload))
      continue;
    const result = payload as Record<string, unknown>;
    if (result.success === false || result.error) continue;
    if (
      typeof result.status === "string" &&
      ["inProgress", "failed", "declined"].includes(result.status)
    )
      continue;
    if (result.type === "fileChange" && Array.isArray(result.changes)) {
      if (result.status !== "completed" && message.toolStatus !== "completed")
        continue;
      const changes: unknown[] = result.changes;
      for (const item of changes) {
        if (!item || typeof item !== "object" || Array.isArray(item)) continue;
        const change = item as Record<string, unknown>;
        if (
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
      typeof result.diff === "string" &&
      result.diff.trim()
    ) {
      const paths = patchPaths(result.diff);
      add(
        {
          ...base(
            paths.length === 1
              ? filename(paths[0])
              : `Changes${paths.length ? ` · ${paths.length} files` : ""}`,
          ),
          kind: "patch",
          source: result.diff,
          paths,
          truncated: !!message.truncated,
        },
        `patch:${paths.length === 1 ? paths[0] : ""}:${result.diff}`,
      );
    }
  }
  return results;
}
