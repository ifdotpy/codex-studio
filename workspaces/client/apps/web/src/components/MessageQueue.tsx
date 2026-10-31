import { useId, useLayoutEffect, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronUp,
  Copy,
  GripVertical,
  ListOrdered,
  Paperclip,
  Pencil,
  Send,
  Trash2,
} from "lucide-react";
import { errorText, type QueueItemDto } from "../api";
import type { Message } from "../types";
import "./message-queue.css";
import { copyText } from "../clipboard/clipboard";

export type LocalQueueItem = Pick<
  Message,
  "assets" | "id" | "requestedDelivery" | "text"
> & { localDelivery: true };
export type QueueItem =
  | (QueueItemDto & { localDelivery?: false })
  | LocalQueueItem;

function isServerQueueItem(item: QueueItem): item is QueueItemDto {
  return item.localDelivery !== true;
}

type QueueAsset = Pick<
  NonNullable<QueueItemDto["assets"]>[number],
  "id" | "name"
>;
function isQueueAsset(value: unknown): value is QueueAsset {
  return (
    typeof value === "object" &&
    value !== null &&
    !Array.isArray(value) &&
    typeof (value as QueueAsset).id === "string" &&
    typeof (value as QueueAsset).name === "string"
  );
}

function displayQueueAssets(item: QueueItem): QueueAsset[] {
  const assets = item.assets ?? [];
  return isServerQueueItem(item) ? assets : assets.filter(isQueueAsset);
}

type Props = {
  items: QueueItem[];
  scope: string;
  onEdit: (item: QueueItemDto, text: string) => Promise<void>;
  onCancel: (item: QueueItemDto) => Promise<void>;
  onSendNow?: (item: QueueItemDto) => Promise<void>;
  onReorder: (ids: string[]) => Promise<void>;
  canReorder: boolean;
  refreshing?: boolean;
  loading?: boolean;
  error?: string;
};

type EditDraft = { item: QueueItemDto; text: string };
const dragType = "application/x-studio-queued-message";

function QueuePreview({
  text,
  index,
  expanded,
  toggle,
}: {
  text: string;
  index: number;
  expanded: boolean;
  toggle: () => void;
}) {
  const preview = useRef<HTMLDivElement>(null);
  const [long, setLong] = useState(false);
  useLayoutEffect(() => {
    const element = preview.current;
    if (!element) return;
    const measure = () => {
      element.classList.remove("expanded");
      const overflow = element.scrollHeight > element.clientHeight + 1;
      if (expanded) element.classList.add("expanded");
      setLong(overflow);
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element.parentElement!);
    return () => observer.disconnect();
  }, [text, expanded]);
  return (
    <>
      <div
        ref={preview}
        className={`message-queue-preview${expanded ? " expanded" : ""}`}
      >
        {text.trimEnd() || "Attachments"}
      </div>
      {long && (
        <button
          type="button"
          className="message-queue-expand"
          aria-label={`${expanded ? "Collapse" : "Expand"} queued message ${index}`}
          aria-expanded={expanded}
          title={expanded ? "Show less" : "Show more"}
          onClick={toggle}
        >
          {expanded ? <ChevronUp size={14} /> : <ChevronDown size={14} />}
          <span className="sr-only">
            {expanded ? "Show less" : "Show more"}
          </span>
        </button>
      )}
    </>
  );
}

export default function MessageQueue(props: Props) {
  return <ScopedMessageQueue key={props.scope} {...props} />;
}

function ScopedMessageQueue(p: Props) {
  const [current, setCurrent] = useState<EditDraft | null>(null);
  const [collapsed, setCollapsed] = useState(false);
  const [busy, setBusy] = useState(false);
  const lock = useRef(false);
  const [error, setError] = useState("");
  const [copied, setCopied] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [dragged, setDragged] = useState<string | null>(null);
  const [dropTarget, setDropTarget] = useState<string | null>(null);
  const listId = useId();
  const textarea = useRef<HTMLTextAreaElement>(null);
  const absent =
    current && !p.items.some((item) => item.id === current.item.id)
      ? current
      : undefined;
  const orphan = p.loading ? undefined : absent;
  const disabled = busy || !!p.refreshing || !!p.loading;

  const dismiss = () => {
    setCurrent(null);
    setError("");
  };
  const beginEdit = (item: QueueItem) => {
    if (!isServerQueueItem(item)) return;
    setCurrent({ item, text: item.text });
    setCollapsed(false);
    setError("");
    setCopied(false);
  };
  const updateDraft = (draft: EditDraft, text: string) => {
    setCurrent({ ...draft, text });
    setCopied(false);
  };
  const mutate = async (operation: () => Promise<void>) => {
    if (lock.current || p.refreshing) return;
    lock.current = true;
    setBusy(true);
    setError("");
    try {
      await operation();
    } catch (reason) {
      setError(errorText(reason));
    } finally {
      lock.current = false;
      setBusy(false);
    }
  };
  const saveEdit = (draft: EditDraft) => {
    if (
      p.loading ||
      !p.items.some((item) => item.id === draft.item.id) ||
      (!draft.text.trim() && !draft.item.assets?.length)
    )
      return;
    void mutate(async () => {
      await p.onEdit(draft.item, draft.text);
      dismiss();
    });
  };
  const move = (id: string, targetIndex: number) => {
    if (
      !p.canReorder ||
      disabled ||
      targetIndex < 0 ||
      targetIndex >= p.items.length
    )
      return;
    const ids = p.items.map((item) => item.id);
    const from = ids.indexOf(id);
    if (from < 0 || from === targetIndex) return;
    ids.splice(from, 1);
    ids.splice(targetIndex, 0, id);
    void mutate(() => p.onReorder(ids));
  };
  const copyDraft = async (draft: EditDraft) => {
    try {
      await copyText(draft.text);
      setCopied(true);
    } catch {
      textarea.current?.focus();
      textarea.current?.select();
      setError(
        "Copy is unavailable. Select and copy the text from the editor.",
      );
    }
  };
  const editor = (draft: EditDraft, missing = false) => (
    <div className="message-queue-editor">
      {missing && (
        <p role="status">
          {p.loading
            ? "Checking the queue…"
            : "This message has left the queue. Your edit remains here. Copy the text to use it again."}
        </p>
      )}
      <textarea
        ref={textarea}
        aria-label="Edit queued message"
        value={draft.text}
        rows={Math.min(6, Math.max(3, draft.text.split("\n").length))}
        disabled={busy}
        autoFocus
        onChange={(event) => updateDraft(draft, event.currentTarget.value)}
        onKeyDown={(event) => {
          if (event.nativeEvent.isComposing) return;
          if (event.key === "Escape") {
            event.preventDefault();
            event.stopPropagation();
            if (!busy) dismiss();
          } else if (
            event.key === "Enter" &&
            (event.metaKey || event.ctrlKey)
          ) {
            event.preventDefault();
            event.stopPropagation();
            if (!missing) saveEdit(draft);
          }
        }}
      />
      <div className="message-queue-editor-actions">
        {!missing && (
          <button
            type="button"
            className="message-queue-save"
            aria-label="Save queued message"
            disabled={
              disabled || (!draft.text.trim() && !draft.item.assets?.length)
            }
            onClick={() => saveEdit(draft)}
          >
            Save
          </button>
        )}
        {missing && (
          <button type="button" onClick={() => void copyDraft(draft)}>
            <Copy size={14} />
            {copied ? "Copied" : "Copy text"}
          </button>
        )}
        <button
          type="button"
          aria-label={missing ? "Discard saved queue edit" : "Cancel edit"}
          disabled={busy}
          onClick={() => dismiss()}
        >
          {missing ? "Discard edit" : "Cancel"}
        </button>
        {!missing && <span>⌘/Ctrl + Enter to save</span>}
      </div>
    </div>
  );

  if (!p.items.length && !current && !p.error && !error) return null;
  return (
    <section
      className="message-queue"
      data-testid="message-queue"
      data-studio-state={error || p.error ? "error" : "paused"}
      aria-label="Message queue"
      aria-busy={disabled}
    >
      <div className="message-queue-heading">
        <button
          type="button"
          className="message-queue-toggle"
          aria-expanded={!collapsed}
          aria-controls={listId}
          onClick={() => setCollapsed(!collapsed)}
        >
          <ListOrdered size={17} />
          <span>
            {p.items.length} queued{" "}
            {p.items.length === 1 ? "message" : "messages"}
          </span>
          {current && <span className="message-queue-draft-tag">Editing</span>}
          {collapsed ? <ChevronUp size={16} /> : <ChevronDown size={16} />}
        </button>
        {disabled && (
          <span className="message-queue-status" role="status">
            {p.loading ? "Checking the queue…" : "Updating…"}
          </span>
        )}
      </div>
      {(error || p.error) && (
        <p className="message-queue-error" role="alert">
          {error || p.error}
        </p>
      )}
      <div id={listId} hidden={collapsed} className="message-queue-body">
        <ol aria-label="Queued messages">
          {p.items.map((item, index) => {
            const draft = current?.item.id === item.id ? current : undefined;
            const isExpanded = expanded === item.id;
            return (
              <li
                key={item.id}
                data-message-id={item.id}
                className={
                  dropTarget === item.id
                    ? "message-queue-item drop-target"
                    : "message-queue-item"
                }
                onDragOver={(event) => {
                  if (!dragged || !p.canReorder || disabled) return;
                  event.preventDefault();
                  event.stopPropagation();
                  event.dataTransfer.dropEffect = "move";
                  setDropTarget(item.id);
                }}
                onDrop={(event) => {
                  if (!dragged || !p.canReorder || disabled) return;
                  event.preventDefault();
                  event.stopPropagation();
                  if (event.dataTransfer.getData(dragType) === dragged)
                    move(dragged, index);
                  setDragged(null);
                  setDropTarget(null);
                }}
              >
                <div className="message-queue-row">
                  <span className="message-queue-number" aria-hidden="true">
                    {index + 1}
                  </span>
                  <div className="message-queue-content">
                    {!draft && (
                      <QueuePreview
                        text={item.text}
                        index={index + 1}
                        expanded={isExpanded}
                        toggle={() => setExpanded(isExpanded ? null : item.id)}
                      />
                    )}
                    {!!item.assets?.length && (
                      <div className="message-queue-assets">
                        {displayQueueAssets(item).map((asset) => (
                          <span key={asset.id}>
                            <Paperclip size={12} />
                            <span>{asset.name}</span>
                          </span>
                        ))}
                      </div>
                    )}
                    {draft && editor(draft)}
                    {isServerQueueItem(item) && item.error && (
                      <p className="message-queue-error" role="alert">
                        {item.error}
                      </p>
                    )}
                  </div>
                  <div className="message-queue-actions">
                    {p.onSendNow && !draft && isServerQueueItem(item) && (
                      <button
                        type="button"
                        disabled={disabled || !!item.localDelivery}
                        aria-label={`Send queued message ${index + 1} now`}
                        title="Send now"
                        onClick={() => void mutate(() => p.onSendNow!(item))}
                      >
                        <Send size={15} />
                      </button>
                    )}
                    {!draft && (
                      <button
                        type="button"
                        disabled={disabled || !!current || !!item.localDelivery}
                        aria-label={`Edit queued message ${index + 1}`}
                        title="Edit"
                        onClick={() => beginEdit(item)}
                      >
                        <Pencil size={15} />
                      </button>
                    )}
                    <button
                      type="button"
                      disabled={disabled || !!item.localDelivery}
                      aria-label={`Delete queued message ${index + 1}`}
                      title="Delete"
                      onClick={() =>
                        void mutate(async () => {
                          if (isServerQueueItem(item)) await p.onCancel(item);
                        })
                      }
                    >
                      <Trash2 size={15} />
                    </button>
                    {p.canReorder && p.items.length > 1 && (
                      <>
                        <button
                          type="button"
                          disabled={disabled || index === 0}
                          aria-label={`Move queued message ${index + 1} up`}
                          title="Move up"
                          onClick={() => move(item.id, index - 1)}
                        >
                          <ChevronUp size={16} />
                        </button>
                        <button
                          type="button"
                          disabled={disabled || index === p.items.length - 1}
                          aria-label={`Move queued message ${index + 1} down`}
                          title="Move down"
                          onClick={() => move(item.id, index + 1)}
                        >
                          <ChevronDown size={16} />
                        </button>
                        <span
                          className="message-queue-grip"
                          title="Drag to reorder"
                          aria-hidden="true"
                          draggable={!disabled}
                          onDragStart={(event) => {
                            event.stopPropagation();
                            event.dataTransfer.setData(dragType, item.id);
                            event.dataTransfer.effectAllowed = "move";
                            setDragged(item.id);
                          }}
                          onDragEnd={() => {
                            setDragged(null);
                            setDropTarget(null);
                          }}
                        >
                          <GripVertical size={15} />
                        </span>
                      </>
                    )}
                  </div>
                </div>
              </li>
            );
          })}
        </ol>
        {p.loading && absent && (
          <div className="message-queue-orphan">{editor(absent, true)}</div>
        )}
        {orphan && (
          <div className="message-queue-orphan">{editor(orphan, true)}</div>
        )}
      </div>
    </section>
  );
}
