import { ActionIcon, Button, Popover, TextInput } from "@mantine/core";
import { Bookmark, ListTree, Search } from "lucide-react";
import { useEffect, useMemo, useState, type RefObject } from "react";
import { syncApi, errorText, save, saved } from "../api";
import type { Message } from "../types";
import "./prompt-navigation.css";

export default function PromptNavigator({
  messages,
  container,
  storageKey,
  jump,
  compact = false,
  agentId,
}: {
  compact?: boolean;
  agentId?: string;
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
  const [search, setSearch] = useState<{
    query: string;
    results: Message[];
    loading: boolean;
    error: string;
    truncated?: boolean;
  }>({ query: "", results: [], loading: false, error: "" });
  const [retry, setRetry] = useState(0);
  const searchQuery = query.trim();
  useEffect(() => {
    if (!opened || !agentId || !searchQuery || onlySaved) return;
    let active = true;
    setSearch({ query: searchQuery, results: [], loading: true, error: "" });
    const timer = setTimeout(() => {
      const params = new URLSearchParams({ id: agentId, q: searchQuery });
      void syncApi(`/api/transcript/search?${params}`)
        .then((result) => {
          if (active)
            setSearch({
              query: searchQuery,
              results: result.results || [],
              loading: false,
              error: "",
              truncated: result.truncated,
            });
        })
        .catch((error) => {
          if (active)
            setSearch({
              query: searchQuery,
              results: [],
              loading: false,
              error: errorText(error),
            });
        });
    }, 250);
    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, [opened, agentId, searchQuery, onlySaved, retry]);
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
  const index = Math.max(
    0,
    prompts.findIndex((prompt) => prompt.id === activeId),
  );
  if (!messages.length) return null;
  const current = prompts[index];
  const searching = !!searchQuery && !onlySaved;
  const searchPending =
    searching && !!agentId && (search.query !== searchQuery || search.loading);
  const searchError =
    searching && search.query === searchQuery ? search.error : "";
  const visible = searching
    ? agentId
      ? search.query === searchQuery
        ? search.results
        : []
      : messages.filter(
          (message) =>
            ["user", "assistant"].includes(message.role) &&
            message.text.toLowerCase().includes(searchQuery.toLowerCase()),
        )
    : prompts.filter(
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
    <nav
      className={`prompt-navigation${compact ? " prompt-navigation-compact" : ""}`}
      aria-label="Conversation prompts"
    >
      <Popover
        opened={opened}
        onChange={setOpened}
        position={compact ? "top-end" : "bottom-start"}
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
            title="Browse prompts."
            aria-expanded={opened}
            onClick={() => setOpened(!opened)}
            leftSection={<ListTree size={14} />}
          >
            {prompts.length
              ? `${index + 1} / ${prompts.length}`
              : "Search chat"}
          </Button>
        </Popover.Target>
        <Popover.Dropdown
          className="prompt-history"
          role="dialog"
          aria-label="Prompt history"
          aria-labelledby=""
        >
          <div className="prompt-history-heading">
            <strong>{searching ? "Search this chat" : "Prompts"}</strong>
            <span>
              {searching ? "All messages" : `${prompts.length} loaded`}
            </span>
          </div>
          <div className="prompt-history-search">
            <TextInput
              aria-label="Search this chat"
              placeholder="Search questions and answers"
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
            {searchPending && (
              <p role="status" className="prompt-history-empty">
                Searching…
              </p>
            )}
            {searchError && (
              <p role="alert" className="prompt-history-empty">
                {searchError}{" "}
                <button
                  type="button"
                  onClick={() => setRetry((value) => value + 1)}
                >
                  Retry search
                </button>
              </p>
            )}
            {visible.map((prompt) => (
              <div className="prompt-history-row" key={prompt.id}>
                <button
                  type="button"
                  className="prompt-history-entry"
                  aria-current={
                    current?.id === prompt.id ? "location" : undefined
                  }
                  onClick={() => go(prompt.id)}
                >
                  <span>
                    {searching
                      ? prompt.role === "user"
                        ? "You"
                        : "Assistant"
                      : prompts.indexOf(prompt) + 1}
                  </span>
                  <span>
                    {(searching ? prompt.excerpt : prompt.text) ||
                      prompt.text ||
                      "Attachment"}
                  </span>
                </button>
                {!searching && (
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
                )}
              </div>
            ))}
            {!visible.length && !searchPending && !searchError && (
              <p className="prompt-history-empty">
                {onlySaved
                  ? "No matching bookmarks."
                  : searching
                    ? "No matching messages."
                    : "No matching prompts."}
              </p>
            )}
            {searching && search.truncated && !searchPending && (
              <p className="prompt-history-empty">
                More results exist. Use a more specific search.
              </p>
            )}
          </div>
        </Popover.Dropdown>
      </Popover>
      {current && !compact && (
        <button
          type="button"
          className="current-prompt"
          title={current.text}
          aria-label="Return to current prompt"
          onClick={() => go(current.id)}
        >
          {current.text || "Attachment"}
        </button>
      )}
    </nav>
  );
}
