import { useEffect, useState } from "react";
import { Button } from "@mantine/core";
import "./rich-preview.css";

export default function CodeBlock({
  source,
  language = "text",
}: {
  source: string;
  language?: string;
}) {
  const [expanded, setExpanded] = useState(false);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    setCopied(false);
    setError("");
  }, [source]);
  return (
    <section className="code-block" aria-label={`${language} code`}>
      <div className="rich-preview-toolbar">
        <span className="rich-preview-label">{language}</span>
        <Button
          size="compact-xs"
          variant="subtle"
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(source);
              setCopied(true);
              setError("");
            } catch {
              setError("Cannot copy. Select the code to copy it.");
            }
          }}
        >
          {copied ? "Copied" : "Copy code"}
        </Button>
        <Button
          size="compact-xs"
          variant="subtle"
          aria-expanded={expanded}
          onClick={() => setExpanded(!expanded)}
        >
          {expanded ? "Collapse code" : "Expand code"}
        </Button>
      </div>
      {error && (
        <p className="rich-preview-error" role="alert">
          {error}
        </p>
      )}
      <pre className="code-block-content" data-expanded={expanded}>
        <code>{source}</code>
      </pre>
    </section>
  );
}
