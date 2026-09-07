import { ActionIcon, Button, Popover, TextInput } from "@mantine/core";
import { ArrowDown, ArrowUp, Bookmark, ListTree, Search } from "lucide-react";
import { useEffect, useMemo, useState, type RefObject } from "react";
import { save, saved } from "../api";
import type { Message } from "../types";
import "./prompt-navigation.css";

export default function PromptNavigator({
  messages,
  container,
  storageKey,
  jump,
}: {
  messages: Message[];
  container: RefObject<HTMLDivElement | null>;
  storageKey: string;
  jump: (id: string) => void;
}) {
  const prompts = useMemo(
    () => messages.filter((item) => item.role === "user" && !item.pending),
    [messages],
  );
  const [activeId, setActiveId] = useState("");
  const [opened, setOpened] = useState(false);
  const [query, setQuery] = useState("");
  const [bookmarks, setBookmarks] = useState<Record<string, boolean>>(() =>
    saved(storageKey, {}),
  );
  const [onlySaved, setOnlySaved] = useState(false);
  useEffect(() => {
    const root = container.current;
    if (!root) return;
    let frame = 0;
    const measure = () => {
      frame = 0;
      const top = root.getBoundingClientRect().top + 24;
      const ids = new Set(prompts.map((prompt) => prompt.id));
      let current = prompts[0]?.id || "";
      for (const element of root.querySelectorAll<HTMLElement>(
        "[data-message]",
      )) {
        if (
          ids.has(element.dataset.message || "") &&
          element.getBoundingClientRect().top <= top
        )
          current = element.dataset.message!;
      }
      setActiveId(current);
    };
    const schedule = () => {
      if (!frame) frame = requestAnimationFrame(measure);
    };
    root.addEventListener("scroll", schedule, { passive: true });
    const resize = new ResizeObserver(schedule);
    resize.observe(root);
    schedule();
    return () => {
      root.removeEventListener("scroll", schedule);
      resize.disconnect();
      cancelAnimationFrame(frame);
    };
  }, [prompts, container]);
  if (!prompts.length) return null;
  const index = Math.max(
    0,
    prompts.findIndex((prompt) => prompt.id === activeId),
  );
  const current = prompts[index];
  const visible = prompts.filter(
    (prompt) =>
      (!onlySaved || bookmarks[prompt.id]) &&
      prompt.text.toLowerCase().includes(query.toLowerCase()),
  );
  const go = (id: string) => {
    jump(id);
    setOpened(false);
  };
  const toggleBookmark = (id: string) => {
    const next = { ...bookmarks };
    if (next[id]) delete next[id];
    else next[id] = true;
    setBookmarks(next);
    save(storageKey, next);
  };
  return (
    <nav className="prompt-navigation" aria-label="Conversation prompts">
      <Popover
        opened={opened}
        onChange={setOpened}
        position="bottom-start"
        width={380}
        withinPortal
        shadow="md"
      >
        <Popover.Target>
          <Button
            size="compact-xs"
            variant="subtle"
            className="prompt-history-toggle"
            aria-label="Browse prompts"
            aria-expanded={opened}
            onClick={() => setOpened(!opened)}
            leftSection={<ListTree size={14} />}
          >
            {index + 1} / {prompts.length}
          </Button>
        </Popover.Target>
        <Popover.Dropdown
          className="prompt-history"
          role="dialog"
          aria-label="Prompt history"
          aria-labelledby=""
        >
          <div className="prompt-history-heading">
            <strong>Prompts</strong>
            <span>{prompts.length} loaded</span>
          </div>
          <div className="prompt-history-search">
            <TextInput
              aria-label="Find a prompt"
              placeholder="Find a prompt"
              value={query}
              onChange={(event) => setQuery(event.currentTarget.value)}
              leftSection={<Search size={14} />}
              size="xs"
            />
            <ActionIcon
              variant={onlySaved ? "light" : "subtle"}
              aria-label="Show bookmarked prompts"
              aria-pressed={onlySaved}
              onClick={() => setOnlySaved(!onlySaved)}
            >
              <Bookmark size={15} />
            </ActionIcon>
          </div>
          <div className="prompt-history-list">
            {visible.map((prompt) => (
              <div className="prompt-history-row" key={prompt.id}>
                <button
                  type="button"
                  className="prompt-history-entry"
                  aria-current={
                    current.id === prompt.id ? "location" : undefined
                  }
                  onClick={() => go(prompt.id)}
                >
                  <span>{prompts.indexOf(prompt) + 1}</span>
                  <span>{prompt.text || "Attachment"}</span>
                </button>
                <ActionIcon
                  size="sm"
                  variant="subtle"
                  aria-label={`${bookmarks[prompt.id] ? "Remove bookmark from" : "Bookmark"} prompt ${prompts.indexOf(prompt) + 1}`}
                  aria-pressed={!!bookmarks[prompt.id]}
                  onClick={() => toggleBookmark(prompt.id)}
                >
                  <Bookmark
                    size={14}
                    fill={bookmarks[prompt.id] ? "currentColor" : "none"}
                  />
                </ActionIcon>
              </div>
            ))}
            {!visible.length && (
              <p className="prompt-history-empty">
                {onlySaved ? "No matching bookmarks." : "No matching prompts."}
              </p>
            )}
          </div>
        </Popover.Dropdown>
      </Popover>
      <button
        type="button"
        className="current-prompt"
        title={current.text}
        aria-label="Return to current prompt"
        onClick={() => go(current.id)}
      >
        {current.text || "Attachment"}
      </button>
      <ActionIcon
        size="sm"
        variant="subtle"
        aria-label="Previous prompt"
        disabled={index === 0}
        onClick={() => go(prompts[index - 1].id)}
      >
        <ArrowUp size={14} />
      </ActionIcon>
      <ActionIcon
        size="sm"
        variant="subtle"
        aria-label="Next prompt"
        disabled={index === prompts.length - 1}
        onClick={() => go(prompts[index + 1].id)}
      >
        <ArrowDown size={14} />
      </ActionIcon>
    </nav>
  );
}
