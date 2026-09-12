import { useEffect, useId, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronUp,
  Copy,
  GripVertical,
  ListOrdered,
  Paperclip,
  Pencil,
  Trash2,
} from "lucide-react";
import { errorText, saved } from "../api";
import { writeLocalDraft } from "../sync/localDraft";
import type { Attachment } from "./ComposerAttachments";
import "./message-queue.css";

export type QueueItem = {
  id: string;
  text: string;
  assets?: Attachment[];
  [key: string]: any;
};

type Props = {
  items: QueueItem[];
  scope: string;
  onEdit: (item: QueueItem, text: string) => Promise<void>;
  onCancel: (item: QueueItem) => Promise<void>;
  onReorder: (ids: string[]) => Promise<void>;
  canReorder: boolean;
  refreshing?: boolean;
  loading?: boolean;
  error?: string;
};

type EditDraft = {
  item: QueueItem;
  text: string;
  revision: string;
  key: string;
  updated: number;
  ancestors?: Array<{ key: string; revision: string }>;
};
type Drafts = { active: string | null; entries: Record<string, EditDraft> };
const dragType = "application/x-studio-queued-message";

function readDrafts(prefix: string): { drafts: Drafts; error: string } {
  const entries: Record<string, EditDraft> = {};
  try {
    for (let index = 0; index < localStorage.length; index++) {
      const key = localStorage.key(index);
      if (!key?.startsWith(prefix)) continue;
      const draft = saved<EditDraft | null>(key, null);
      if (
        draft?.key === key &&
        typeof draft.item?.id === "string" &&
        typeof draft.item.text === "string" &&
        typeof draft.text === "string" &&
        typeof draft.revision === "string" &&
        typeof draft.updated === "number"
      )
        entries[key] = draft;
    }
    const latest = Object.values(entries).sort(
      (a, b) => b.updated - a.updated,
    )[0];
    return { drafts: { active: latest?.key || null, entries }, error: "" };
  } catch {
    return {
      drafts: { active: null, entries },
      error: "Saved queue edits could not be read. Keep this page open.",
    };
  }
}

export default function MessageQueue(props: Props) {
  return <ScopedMessageQueue key={props.scope} {...props} />;
}

function ScopedMessageQueue(p: Props) {
  const storageKey = `codex-queue-edit:${encodeURIComponent(p.scope)}:`;
  const [writer] = useState(() => crypto.randomUUID());
  const [recovery] = useState(() => readDrafts(storageKey));
  const [drafts, setDrafts] = useState(recovery.drafts);
  const [storageError, setStorageError] = useState(recovery.error);
  const draftsRef = useRef(drafts);
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
  const current = drafts.active ? drafts.entries[drafts.active] : undefined;
  const absent =
    current && !p.items.some((item) => item.id === current.item.id)
      ? current
      : undefined;
  const orphan = p.loading ? undefined : absent;
  const disabled = busy || !!p.refreshing || !!p.loading;

  const retain = (next: Drafts) => {
    draftsRef.current = next;
    setDrafts(next);
  };
  useEffect(() => {
    const refresh = (event: StorageEvent) => {
      if (event.key !== null && !event.key.startsWith(storageKey)) return;
      const loaded = readDrafts(storageKey);
      const previous = draftsRef.current;
      const local = previous.active
        ? previous.entries[previous.active]
        : undefined;
      // External writes never replace text in the open editor.
      retain({
        active: previous.active,
        entries: {
          ...loaded.drafts.entries,
          ...Object.fromEntries(
            Object.entries(previous.entries).filter(([key]) =>
              key.startsWith(`${storageKey}${writer}:`),
            ),
          ),
          ...(local ? { [local.key]: local } : {}),
        },
      });
      if (loaded.error) setStorageError(loaded.error);
    };
    window.addEventListener("storage", refresh);
    return () => window.removeEventListener("storage", refresh);
  }, [storageKey, writer]);
  const persist = (draft: EditDraft, previous?: EditDraft) => {
    let problem = writeLocalDraft(draft.key, draft);
    const entries = { ...draftsRef.current.entries, [draft.key]: draft };
    // Every key is immutable. Retire an owned version only after its replacement exists.
    if (!problem && previous?.key.startsWith(`${storageKey}${writer}:`)) {
      problem = writeLocalDraft(previous.key, null);
      if (!problem) delete entries[previous.key];
    }
    setStorageError(problem || "");
    retain({ active: draft.key, entries });
  };
  const dismiss = (draft: EditDraft) => {
    const previous = draftsRef.current;
    if (previous.entries[draft.key]?.revision !== draft.revision) return;
    const entries = { ...previous.entries };
    let problem: string | null = null;
    for (const target of [draft, ...(draft.ancestors || [])]) {
      if (!target.key.startsWith(storageKey)) continue;
      const stored = saved<EditDraft | null>(target.key, null);
      if (stored && stored.revision !== target.revision) {
        entries[target.key] = stored;
        continue;
      }
      problem = writeLocalDraft(target.key, null) || problem;
      if (entries[target.key]?.revision === target.revision)
        delete entries[target.key];
    }
    setStorageError(problem || "");
    retain({
      active: previous.active === draft.key ? null : previous.active,
      entries,
    });
    setError("");
  };
  const beginEdit = (item: QueueItem) => {
    persist({
      item,
      text: item.text,
      revision: crypto.randomUUID(),
      key: `${storageKey}${writer}:${crypto.randomUUID()}`,
      updated: Date.now(),
    });
    setCollapsed(false);
    setError("");
    setCopied(false);
  };
  const updateDraft = (draft: EditDraft, text: string) => {
    const own = draft.key.startsWith(`${storageKey}${writer}:`);
    persist(
      {
        ...draft,
        text,
        revision: crypto.randomUUID(),
        key: `${storageKey}${writer}:${crypto.randomUUID()}`,
        updated: Date.now(),
        ancestors: own
          ? draft.ancestors
          : [
              ...(draft.ancestors || []),
              { key: draft.key, revision: draft.revision },
            ],
      },
      own ? draft : undefined,
    );
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
      dismiss(draft);
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
      await navigator.clipboard.writeText(draft.text);
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
            if (!busy) dismiss(draft);
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
          onClick={() => dismiss(draft)}
        >
          {missing ? "Discard edit" : "Cancel"}
        </button>
        {!missing && <span>⌘/Ctrl + Enter to save</span>}
      </div>
    </div>
  );

  const alternatives = Object.values(drafts.entries).filter(
    (draft) =>
      draft.key !== current?.key &&
      !current?.ancestors?.some(
        (ancestor) =>
          ancestor.key === draft.key && ancestor.revision === draft.revision,
      ) &&
      !(
        draft.item.id === current?.item.id &&
        draft.text === current.text &&
        draft.item.text === current.item.text
      ),
  );
  if (
    !p.items.length &&
    !current &&
    !alternatives.length &&
    !p.error &&
    !error &&
    !storageError
  )
    return null;
  return (
    <section
      className="message-queue"
      data-testid="message-queue"
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
          {current && (
            <span className="message-queue-draft-tag">
              {storageError ? "Edit not saved" : "Edit saved"}
            </span>
          )}
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
      {storageError && (
        <p className="message-queue-error" role="alert">
          {storageError}
        </p>
      )}
      <div id={listId} hidden={collapsed} className="message-queue-body">
        {!!alternatives.length && (
          <details className="message-queue-orphan">
            <summary>Other saved edits ({alternatives.length})</summary>
            {alternatives.map((draft, index) => (
              <div key={draft.key} className="message-queue-editor">
                <p>{draft.text || "Attachments"}</p>
                <div className="message-queue-editor-actions">
                  <button
                    type="button"
                    disabled={disabled}
                    aria-label={`Open saved queue edit ${index + 1}`}
                    onClick={() => {
                      retain({ ...draftsRef.current, active: draft.key });
                      setCopied(false);
                      setError("");
                    }}
                  >
                    Open edit
                  </button>
                  <button
                    type="button"
                    disabled={disabled}
                    aria-label={`Discard saved queue edit ${index + 1}`}
                    onClick={() => dismiss(draft)}
                  >
                    Discard edit
                  </button>
                </div>
              </div>
            ))}
          </details>
        )}
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
                      <button
                        type="button"
                        className={`message-queue-preview${isExpanded ? " expanded" : ""}`}
                        aria-label={`${isExpanded ? "Collapse" : "Expand"} queued message ${index + 1}`}
                        aria-expanded={isExpanded}
                        onClick={() => setExpanded(isExpanded ? null : item.id)}
                      >
                        {item.text || "Attachments"}
                      </button>
                    )}
                    {!!item.assets?.length && (
                      <div className="message-queue-assets">
                        {item.assets.map((asset) => (
                          <span key={asset.id}>
                            <Paperclip size={12} />
                            <span>{asset.name}</span>
                          </span>
                        ))}
                      </div>
                    )}
                    {draft && editor(draft)}
                  </div>
                  <div className="message-queue-actions">
                    {!draft && (
                      <button
                        type="button"
                        disabled={disabled || !!current}
                        aria-label={`Edit queued message ${index + 1}`}
                        title="Edit"
                        onClick={() => beginEdit(item)}
                      >
                        <Pencil size={15} />
                      </button>
                    )}
                    <button
                      type="button"
                      disabled={disabled}
                      aria-label={`Delete queued message ${index + 1}`}
                      title="Delete"
                      onClick={() =>
                        void mutate(async () => {
                          await p.onCancel(item);
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
