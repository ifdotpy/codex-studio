import { useState } from "react";
import { diffTotals, type FileDiff as RecordedDiff } from "./fileChangeModel";
import "./file-diff.css";

export function DiffCounts({ files }: { files: RecordedDiff[] }) {
  if (!files.length || files.some((file) => !file.parsed)) return null;
  const { added, removed } = diffTotals(files);
  return (
    <span
      className="file-diff-counts"
      aria-label={`${added} added lines, ${removed} removed lines`}
    >
      (<span className="file-diff-added">+{added}</span>{" "}
      <span className="file-diff-removed">-{removed}</span>)
    </span>
  );
}

function FileDiffBody({
  file,
  showCounts,
  showPath,
}: {
  file: RecordedDiff;
  showCounts: boolean;
  showPath: boolean;
}) {
  const [expanded, setExpanded] = useState(false);
  let hunks = 0;
  const lines = file.lines.flatMap((line) => {
    if (line.kind !== "meta" || !line.text.startsWith("@@")) return [line];
    return hunks++ ? [{ ...line, text: "⋮" }] : [];
  });
  const rawLines = file.parsed ? [] : file.source.split("\n");
  const length = file.parsed ? lines.length : rawLines.length;
  const visible = expanded ? length : Math.min(length, 20);
  return (
    <section
      className="file-diff"
      aria-label={`Changes to ${file.path || "file"}`}
    >
      {showPath && (
        <div className="file-diff-heading">
          <span className="file-diff-path">
            {file.path || "File change"}
            {file.movePath && <> → {file.movePath}</>}
          </span>
          {showCounts && <DiffCounts files={[file]} />}
        </div>
      )}
      {file.parsed ? (
        <div className="file-diff-lines">
          {lines.slice(0, visible).map((line, index) => (
            <div className="file-diff-line" data-kind={line.kind} key={index}>
              <span className="file-diff-number">{line.line ?? ""}</span>
              <span className="file-diff-sign">
                {line.kind === "add" ? "+" : line.kind === "delete" ? "-" : " "}
              </span>
              <code className="file-diff-text">{line.text || "\u00a0"}</code>
            </div>
          ))}
          {!length && <p className="file-diff-notice">No changed lines.</p>}
        </div>
      ) : (
        <>
          <p className="file-diff-notice">
            The saved patch could not be parsed. Original text:
          </p>
          <pre className="file-diff-raw">
            {rawLines.slice(0, visible).join("\n")}
          </pre>
        </>
      )}
      {length > 20 && (
        <button
          type="button"
          className="file-diff-expand"
          aria-expanded={expanded}
          onClick={() => setExpanded(!expanded)}
        >
          {expanded ? "Show fewer lines" : `Show all ${length} lines`}
        </button>
      )}
    </section>
  );
}

export default function FileDiff({
  files,
  showCounts = true,
  showPaths = true,
}: {
  files: RecordedDiff[];
  showCounts?: boolean;
  showPaths?: boolean;
}) {
  return (
    <div className="file-diff-list">
      {files.map((file, index) => (
        <FileDiffBody
          file={file}
          showCounts={showCounts}
          showPath={showPaths}
          key={`${file.path}:${index}`}
        />
      ))}
    </div>
  );
}
