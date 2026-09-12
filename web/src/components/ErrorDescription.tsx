import { useState } from "react";
import { displayError, errorDetails } from "../errorPresentation";

// Phrasing-only markup remains valid inside paragraphs and status labels.
export default function ErrorDescription({
  value,
  summary,
  className,
  role = "alert",
}: {
  value: unknown;
  summary?: string;
  className?: string;
  role?: "alert" | "status";
}) {
  const [expanded, setExpanded] = useState(false);
  const message = summary || displayError(value);
  const details = errorDetails(value);
  if (!message && !details) return null;
  return (
    <span className={className} role={role}>
      <span>{message}</span>
      {details && details !== message && (
        <>
          {" "}
          <button
            type="button"
            aria-expanded={expanded}
            onClick={() => setExpanded(!expanded)}
          >
            {expanded ? "Hide error details" : "Error details"}
          </button>
          {expanded && (
            <span
              style={{
                display: "block",
                whiteSpace: "pre-wrap",
                overflowWrap: "anywhere",
              }}
            >
              {details}
            </span>
          )}
        </>
      )}
    </span>
  );
}
