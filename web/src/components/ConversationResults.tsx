import { Button, Modal } from "@mantine/core";
import { ChevronRight, FileDiff, FileText, Image, Shapes } from "lucide-react";
import { useMemo, useState } from "react";
import type { Message } from "../types";
import FilePreview from "./FilePreview";
import RichPreview from "./RichPreview";
import {
  conversationResults,
  type ConversationResult,
} from "./conversationResultModel";
import "./conversation-results.css";

export default function ConversationResults({
  messages,
  agentId,
  onJump,
}: {
  messages: Message[];
  agentId?: string;
  onJump?: (messageId: string) => void;
}) {
  const results = useMemo(() => conversationResults(messages), [messages]);
  const [open, setOpen] = useState(false);
  const [all, setAll] = useState(false);
  const [selection, setSelection] = useState<{ id: string; agent?: string }>();
  // Selection cannot carry an old conversation's result into another chat.
  const selected =
    selection?.agent === agentId
      ? results.find((result) => result.id === selection?.id)
      : undefined;
  const visible = all ? results : results.slice(0, 6);
  if (!results.length) return null;
  const icon = (result: ConversationResult) =>
    result.kind === "patch" ? (
      <FileDiff size={14} />
    ) : result.kind === "html" || result.kind === "mermaid" ? (
      <Shapes size={14} />
    ) : (result.kind === "file" || result.kind === "asset") && result.image ? (
      <Image size={14} />
    ) : (
      <FileText size={14} />
    );
  const file = selected?.kind === "file" || selected?.kind === "asset";
  return (
    <section
      className="conversation-results"
      aria-label="Results in these messages"
    >
      <button
        type="button"
        className="conversation-results-toggle"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <ChevronRight size={13} className={open ? "expanded" : ""} />
        <span>Results</span>
        <span className="conversation-results-count">{results.length}</span>
      </button>
      {open && (
        <div className="conversation-results-list">
          {visible.map((result) => (
            <button
              type="button"
              className="conversation-result"
              key={result.id}
              title={
                result.kind === "file"
                  ? result.path
                  : result.kind === "patch"
                    ? result.paths.join("\n")
                    : result.label
              }
              disabled={result.kind === "file" && !agentId}
              onClick={() => setSelection({ id: result.id, agent: agentId })}
            >
              {icon(result)}
              <span>{result.label}</span>
              {result.kind === "patch" && <small>Patch</small>}
            </button>
          ))}
          {results.length > 6 && (
            <button
              type="button"
              className="conversation-results-more"
              onClick={() => setAll(!all)}
            >
              {all ? "Show fewer" : `Show ${results.length - 6} more`}
            </button>
          )}
        </div>
      )}
      {file && (
        <FilePreview
          target={
            selected.kind === "asset"
              ? { asset: selected.asset }
              : { agent: agentId, path: selected.path, line: selected.line }
          }
          onClose={() => setSelection(undefined)}
        />
      )}
      <Modal
        opened={!!selected && !file}
        onClose={() => setSelection(undefined)}
        title={selected?.label}
        size="xl"
        className="conversation-result-modal"
      >
        {selected && !file && (
          <>
            {selected.kind === "patch" ? (
              <>
                <p className="conversation-results-note">
                  Recorded patch from this message.
                  {selected.truncated ? " The saved content is clipped." : ""}
                </p>
                {!!selected.paths.length && (
                  <p className="conversation-results-paths">
                    {selected.paths.join("\n")}
                  </p>
                )}
                <pre className="conversation-results-patch">
                  <code>{selected.source}</code>
                </pre>
              </>
            ) : (
              (selected.kind === "html" || selected.kind === "mermaid") && (
                <RichPreview kind={selected.kind} source={selected.source} />
              )
            )}
            {onJump && (
              <Button
                size="compact-sm"
                variant="subtle"
                onClick={() => {
                  onJump(selected.messageId);
                  setSelection(undefined);
                }}
              >
                Show message
              </Button>
            )}
          </>
        )}
      </Modal>
    </section>
  );
}
