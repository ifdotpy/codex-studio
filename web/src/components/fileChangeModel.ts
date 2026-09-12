export interface DiffLine {
  kind: "context" | "add" | "delete" | "meta";
  text: string;
  line?: number;
}

export interface FileDiff {
  path: string;
  movePath?: string;
  kind: "add" | "delete" | "update" | "unknown";
  lines: DiffLine[];
  added: number;
  removed: number;
  parsed: boolean;
  source: string;
}

function sourceLines(source: string): string[] {
  if (!source) return [];
  const lines = source.split(/\r?\n/);
  if (lines.at(-1) === "") lines.pop();
  return lines;
}

function fallback(
  source: string,
  path: string,
  kind: FileDiff["kind"] = "unknown",
): FileDiff {
  return { path, kind, source, lines: [], added: 0, removed: 0, parsed: false };
}

function headerPath(header: string): string {
  const text = header.slice(4).split("\t")[0];
  // Git quotes paths containing special characters. Decode only valid escapes.
  let path = text;
  if (text.startsWith('"')) {
    try {
      path = JSON.parse(text);
    } catch {
      return text;
    }
  }
  return path.replace(/^[ab]\//, "");
}

const hunkHeader = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?:.*)$/;
const metadata =
  /^(?:diff --git |index |old mode |new mode |new file mode |deleted file mode |similarity index |dissimilarity index |rename from |rename to )/;

function parseFile(source: string, requestedPath = ""): FileDiff {
  const result = fallback(source, requestedPath);
  const input = sourceLines(source);
  let oldPath = "",
    newPath = "",
    oldEnd = 0,
    newEnd = 0,
    hunks = 0;
  let renameFrom = "",
    renameTo = "",
    emptyKind: FileDiff["kind"] = "unknown";
  const lines: DiffLine[] = [];
  let added = 0,
    removed = 0;
  for (let i = 0; i < input.length; ) {
    const text = input[i];
    if (
      text.startsWith("--- ") &&
      input[i + 1]?.startsWith("+++ ") &&
      !oldPath &&
      !hunks
    ) {
      oldPath = headerPath(text);
      newPath = headerPath(input[i + 1]);
      if (!oldPath || !newPath) return result;
      result.path =
        requestedPath || (oldPath === "/dev/null" ? newPath : oldPath);
      result.kind =
        oldPath === "/dev/null"
          ? "add"
          : newPath === "/dev/null"
            ? "delete"
            : "update";
      i += 2;
      continue;
    }
    const match = text.match(hunkHeader);
    if (match) {
      const [oldStart, oldCount, newStart, newCount] = [
        Number(match[1]),
        Number(match[2] ?? 1),
        Number(match[3]),
        Number(match[4] ?? 1),
      ];
      if (
        (oldPath === "/dev/null" && (oldStart !== 0 || oldCount !== 0)) ||
        (newPath === "/dev/null" && (newStart !== 0 || newCount !== 0)) ||
        ![
          oldStart,
          oldCount,
          newStart,
          newCount,
          oldStart + oldCount,
          newStart + newCount,
        ].every(Number.isSafeInteger) ||
        (oldCount > 0 && oldStart === 0) ||
        (newCount > 0 && newStart === 0) ||
        (hunks > 0 && (oldStart < oldEnd || newStart < newEnd)) ||
        (oldCount === 0 && newCount === 0)
      )
        return result;
      lines.push({ kind: "meta", text });
      let oldLine = oldStart,
        newLine = newStart;
      oldEnd = oldStart + oldCount;
      newEnd = newStart + newCount;
      i++;
      let canMark = false;
      while (i < input.length) {
        const body = input[i];
        if (body === "\\ No newline at end of file") {
          if (!canMark) return result;
          lines.push({ kind: "meta", text: body });
          canMark = false;
          i++;
          continue;
        }
        if (oldLine === oldEnd && newLine === newEnd) break;
        if (body.startsWith(" ") && oldLine < oldEnd && newLine < newEnd) {
          lines.push({ kind: "context", text: body.slice(1), line: newLine++ });
          oldLine++;
        } else if (body.startsWith("-") && oldLine < oldEnd) {
          lines.push({ kind: "delete", text: body.slice(1), line: oldLine++ });
          removed++;
        } else if (body.startsWith("+") && newLine < newEnd) {
          lines.push({ kind: "add", text: body.slice(1), line: newLine++ });
          added++;
        } else return result;
        canMark = true;
        i++;
      }
      if (oldLine !== oldEnd || newLine !== newEnd) return result;
      hunks++;
      continue;
    }
    if (metadata.test(text) && !hunks) {
      if (text.startsWith("diff --git ")) {
        const samePath = text.match(/^diff --git a\/(.+) b\/\1$/);
        if (samePath) result.path ||= samePath[1];
      }
      if (text.startsWith("new file mode ")) emptyKind = "add";
      if (text.startsWith("deleted file mode ")) emptyKind = "delete";
      if (text.startsWith("rename from ")) renameFrom = text.slice(12);
      if (text.startsWith("rename to ")) renameTo = text.slice(10);
      i++;
      continue;
    }
    return result;
  }
  if (
    !hunks &&
    !(renameFrom && renameTo) &&
    !(result.path && !oldPath && emptyKind !== "unknown")
  )
    return result;
  if (!hunks && emptyKind !== "unknown") result.kind = emptyKind;
  if (result.kind === "unknown") result.kind = "update";
  result.path ||= renameFrom;
  if (renameTo) result.movePath = renameTo;
  return { ...result, lines, added, removed, parsed: true };
}

/** Read recorded unified diffs, without consulting the current filesystem. */
export function unifiedDiff(source: string, path = ""): FileDiff[] {
  if (!source) return [];
  const input = sourceLines(source);
  const chunks: string[] = [];
  const rawLines = source.match(/[^\n]*\n|[^\n]+$/g) ?? [];
  let chunk: string[] = [],
    oldLeft = 0,
    newLeft = 0,
    seenHeader = false;
  for (let i = 0; i < input.length; i++) {
    const text = input[i];
    const gitStart = text.startsWith("diff --git ");
    const pairStart =
      oldLeft === 0 &&
      newLeft === 0 &&
      text.startsWith("--- ") &&
      input[i + 1]?.startsWith("+++ ");
    if ((gitStart || (pairStart && seenHeader)) && chunk.length) {
      chunks.push(chunk.join(""));
      chunk = [];
      seenHeader = false;
      oldLeft = newLeft = 0;
    }
    if (pairStart) seenHeader = true;
    const match = text.match(hunkHeader);
    if (match) {
      oldLeft = Number(match[2] ?? 1);
      newLeft = Number(match[4] ?? 1);
    } else if (oldLeft || newLeft) {
      if (text.startsWith(" ")) {
        oldLeft--;
        newLeft--;
      } else if (text.startsWith("-")) oldLeft--;
      else if (text.startsWith("+")) newLeft--;
    }
    chunk.push(rawLines[i]);
  }
  if (chunk.length) chunks.push(chunk.join(""));
  return chunks.map((part) => parseFile(part, chunks.length === 1 ? path : ""));
}

/** Native add/delete diffs contain raw file contents, including literal +/- prefixes. */
export function fileChanges(value: unknown): FileDiff[] {
  if (!Array.isArray(value)) return [];
  return value.flatMap((item): FileDiff[] => {
    if (!item || typeof item !== "object" || typeof item.path !== "string")
      return [];
    const source = typeof item.diff === "string" ? item.diff : "";
    const type =
      typeof item.kind === "object" && item.kind ? item.kind.type : item.kind;
    const kind = ["add", "delete", "update"].includes(type)
      ? (type as FileDiff["kind"])
      : "unknown";
    if (typeof item.diff !== "string")
      return [fallback(source, item.path, kind)];
    if (kind === "add" || kind === "delete") {
      const lines: DiffLine[] = sourceLines(source).map((text, i) => ({
        kind,
        text,
        line: i + 1,
      }));
      return [
        {
          path: item.path,
          kind,
          source,
          lines,
          added: kind === "add" ? lines.length : 0,
          removed: kind === "delete" ? lines.length : 0,
          parsed: true,
        },
      ];
    }
    if (item.kind != null && kind === "unknown")
      return [fallback(source, item.path)];
    const move = item.kind?.move_path ?? item.kind?.movePath;
    const movePath = typeof move === "string" && move ? move : undefined;
    const suffix = movePath ? `\n\nMoved to: ${movePath}` : "";
    const patch =
      suffix && source.endsWith(suffix)
        ? source.slice(0, -suffix.length)
        : source;
    const parsed = parseFile(patch, item.path);
    if (kind === "update") parsed.kind = kind;
    if (movePath) {
      parsed.movePath = movePath;
      if (!patch) parsed.parsed = true;
    }
    parsed.source = source;
    return [parsed];
  });
}

export function diffTotals(files: FileDiff[]): {
  added: number;
  removed: number;
} {
  return files.reduce(
    (total, file) => ({
      added: total.added + file.added,
      removed: total.removed + file.removed,
    }),
    { added: 0, removed: 0 },
  );
}

export function changeLabel(files: FileDiff[], status: string): string {
  const normalized = status.toLowerCase().replace(/[_ -]/g, "");
  if (["inprogress", "running", "pending", "started"].includes(normalized))
    return "Applying patch";
  if (["failed", "error"].includes(normalized)) return "Failed to apply patch";
  if (["declined", "rejected"].includes(normalized)) return "Patch declined";
  if (["cancelled", "canceled"].includes(normalized)) return "Patch cancelled";
  if (normalized === "interrupted") return "Patch interrupted";
  if (status !== "completed") return "File changes";
  if (files.length !== 1)
    return files.length ? `Edited ${files.length} files` : "Edited files";
  const file = files[0];
  const verb =
    file.kind === "add"
      ? "Added"
      : file.kind === "delete"
        ? "Deleted"
        : "Edited";
  return `${verb} ${file.path}${file.movePath ? ` → ${file.movePath}` : ""}`;
}
