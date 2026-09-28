import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "../api";

export interface SkillItem {
  name: string;
  description: string;
  path: string;
}

interface SkillCatalog {
  skills: SkillItem[];
  errors: string[];
}

interface CacheEntry {
  expiresAt: number;
  value?: SkillCatalog;
  pending?: Promise<SkillCatalog>;
  failed?: boolean;
}

const CACHE_TTL_MS = 2 * 60 * 1000;
const CACHE_MAX_ENTRIES = 48;
const MAX_VISIBLE_SKILLS = 8;
const skillCatalogCache = new Map<string, CacheEntry>();

function catalogFor(key: string, agentId: string): Promise<SkillCatalog> {
  const existing = skillCatalogCache.get(key);
  if (existing?.value && existing.expiresAt > Date.now())
    return Promise.resolve(existing.value);
  if (existing?.pending) return existing.pending;
  if (existing?.failed && existing.expiresAt > Date.now())
    return Promise.reject(new Error("Skill catalog unavailable"));

  const params = new URLSearchParams({ agent: agentId });
  const entry: CacheEntry = { expiresAt: Date.now() + CACHE_TTL_MS };
  const pending = api<SkillCatalog>(`/api/skills?${params}`)
    .then((result) => {
      const value = {
        skills: Array.isArray(result.skills) ? result.skills : [],
        errors: Array.isArray(result.errors) ? result.errors : [],
      };
      entry.value = value;
      entry.expiresAt = Date.now() + CACHE_TTL_MS;
      entry.pending = undefined;
      return value;
    })
    .catch((error: unknown) => {
      entry.pending = undefined;
      entry.failed = true;
      entry.expiresAt = Date.now() + CACHE_TTL_MS;
      throw error;
    });
  entry.pending = pending;
  skillCatalogCache.delete(key);
  skillCatalogCache.set(key, entry);
  while (skillCatalogCache.size > CACHE_MAX_ENTRIES)
    skillCatalogCache.delete(skillCatalogCache.keys().next().value!);
  return pending;
}

interface TokenRange {
  start: number;
  end: number;
  caret: number;
  query: string;
  signature: string;
}

function tokenAtCaret(value: string, caret: number): TokenRange | null {
  const before = value.slice(0, caret);
  const match = /(^|[^\p{L}\p{N}_])\$([\p{L}\p{N}_-]*)$/u.exec(before);
  if (!match) return null;
  const start = before.length - match[2].length - 1;
  const query = match[2];
  if (/^\d/.test(query)) return null;
  let end = caret;
  while (end < value.length && /[\p{L}\p{N}_-]/u.test(value[end])) end++;
  if (/^\d/.test(value.slice(start + 1, end))) return null;
  return {
    start,
    end,
    caret,
    query,
    signature: `${start}:${end}:${caret}:${query}`,
  };
}

export function useSkillAutocomplete({
  enabled,
  agentId,
  workspace,
  account,
  cwd,
  provider,
  draft,
  input,
  insert,
}: {
  enabled: boolean;
  agentId: string;
  workspace: string;
  account: string;
  cwd: string;
  provider: string;
  draft: string;
  input: React.RefObject<HTMLTextAreaElement | null>;
  insert: (text: string, start: number, end: number) => void;
}) {
  const scopeKey = JSON.stringify([workspace, account, cwd, provider]);
  const conversationKey = `${agentId}:${scopeKey}`;
  const [range, setRange] = useState<TokenRange | null>(null);
  const [catalog, setCatalog] = useState<SkillCatalog | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState(false);
  const [selected, setSelected] = useState(-1);
  const dismissed = useRef("");
  const visibleConversation = useRef(conversationKey);
  const scopeIsCurrent = visibleConversation.current === conversationKey;

  const updateRange = useCallback(() => {
    if (!enabled) {
      setRange(null);
      return;
    }
    const element = input.current;
    if (!element || element.selectionStart !== element.selectionEnd) {
      dismissed.current = "";
      setRange(null);
      return;
    }
    const next = tokenAtCaret(element.value, element.selectionStart);
    if (!next) {
      dismissed.current = "";
      setRange(null);
      return;
    }
    if (dismissed.current === next.signature) {
      setRange(null);
      return;
    }
    dismissed.current = "";
    setRange(next);
  }, [enabled, input]);

  useEffect(() => {
    if (visibleConversation.current !== conversationKey) {
      visibleConversation.current = conversationKey;
      dismissed.current = "";
      setRange(null);
      setCatalog(null);
      setLoading(false);
      setLoadError(false);
      setSelected(-1);
    }
    updateRange();
  }, [conversationKey, draft, updateRange]);

  useEffect(() => {
    setSelected(-1);
  }, [range?.signature]);

  useEffect(() => {
    if (!enabled || !range || catalog || !scopeIsCurrent) return;
    let current = true;
    setLoading(true);
    setLoadError(false);
    void catalogFor(scopeKey, agentId)
      .then((value) => {
        if (current && visibleConversation.current === conversationKey)
          setCatalog(value);
      })
      .catch(() => {
        if (current && visibleConversation.current === conversationKey)
          setLoadError(true);
      })
      .finally(() => {
        if (current && visibleConversation.current === conversationKey)
          setLoading(false);
      });
    return () => {
      current = false;
    };
  }, [
    enabled,
    range,
    catalog,
    scopeKey,
    agentId,
    conversationKey,
    scopeIsCurrent,
  ]);

  const matches = useMemo(() => {
    if (!catalog || !range) return [];
    const query = range.query.toLocaleLowerCase();
    return catalog.skills
      .filter((skill) => skill.name.toLocaleLowerCase().includes(query))
      .slice(0, MAX_VISIBLE_SKILLS);
  }, [catalog, range]);

  const choose = useCallback(
    (skill: SkillItem) => {
      if (!range) return;
      dismissed.current = range.signature;
      setRange(null);
      const suffix = input.current?.value.slice(range.end, range.end + 1) || "";
      insert(
        `$${skill.name}${/\s/.test(suffix) ? "" : " "}`,
        range.start,
        range.end,
      );
    },
    [input, insert, range],
  );

  const onKeyDown = useCallback(
    (event: React.KeyboardEvent<HTMLTextAreaElement>) => {
      if (
        !enabled ||
        !range ||
        event.nativeEvent.isComposing ||
        event.keyCode === 229 ||
        event.shiftKey ||
        event.altKey ||
        event.ctrlKey ||
        event.metaKey
      )
        return false;
      if (event.key === "Escape") {
        event.preventDefault();
        dismissed.current = range.signature;
        setRange(null);
        return true;
      }
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        if (!matches.length) return false;
        event.preventDefault();
        setSelected((current) =>
          event.key === "ArrowDown"
            ? current < matches.length - 1
              ? current + 1
              : 0
            : current > 0
              ? current - 1
              : matches.length - 1,
        );
        return true;
      }
      if (
        (event.key === "Enter" || event.key === "Tab") &&
        selected >= 0 &&
        matches[selected]
      ) {
        event.preventDefault();
        choose(matches[selected]);
        return true;
      }
      return false;
    },
    [choose, enabled, matches, range, selected],
  );

  return {
    range: scopeIsCurrent ? range : null,
    matches: scopeIsCurrent ? matches : [],
    selected: scopeIsCurrent ? selected : -1,
    loading: scopeIsCurrent && loading,
    loadError: scopeIsCurrent && loadError,
    hasErrors: scopeIsCurrent && !!catalog?.errors.length,
    onKeyDown,
    updateRange,
    choose,
  };
}
