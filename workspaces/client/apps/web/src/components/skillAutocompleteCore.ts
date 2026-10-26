import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { GetResult } from "../api";

export type SkillCatalog = GetResult<"/api/skills">;
export type SkillItem = SkillCatalog["skills"][number];

export type SkillCatalogReader = (
  scope: string,
  agentId: string,
) => Promise<SkillCatalog>;
const MAX_VISIBLE_SKILLS = 8;
export function normalizeSkillCatalog(result: SkillCatalog): SkillCatalog {
  return {
    skills: Array.isArray(result.skills) ? result.skills : [],
    errors: Array.isArray(result.errors) ? result.errors : [],
  };
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
  const match = /(^|[^\p{L}\p{N}_])\$([\p{L}\p{N}_:-]*)$/u.exec(before);
  if (!match) return null;
  const start = before.length - match[2].length - 1;
  const query = match[2];
  if (/^\d/.test(query)) return null;
  let end = caret;
  while (end < value.length && /[\p{L}\p{N}_:-]/u.test(value[end])) end++;
  if (/^\d/.test(value.slice(start + 1, end))) return null;
  return {
    start,
    end,
    caret,
    query,
    signature: `${start}:${end}:${caret}:${query}`,
  };
}

export function useSkillAutocompleteWithReader(
  {
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
  },
  readCatalog: SkillCatalogReader,
  ownerKey: string,
) {
  const scopeKey = JSON.stringify([workspace, account, cwd, provider]);
  const conversationKey = JSON.stringify([ownerKey, agentId, scopeKey]);
  const [range, setRange] = useState<TokenRange | null>(null);
  const [catalog, setCatalog] = useState<SkillCatalog | null>(null);
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState(false);
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
    }
    updateRange();
  }, [conversationKey, draft, updateRange]);

  useEffect(() => {
    if (!enabled || !range || !scopeIsCurrent) return;
    let current = true;
    setLoading(true);
    setLoadError(false);
    void readCatalog(scopeKey, agentId)
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
    range?.start,
    scopeKey,
    agentId,
    conversationKey,
    scopeIsCurrent,
    readCatalog,
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

  const dismiss = useCallback(() => {
    dismissed.current = range?.signature || "";
    setRange(null);
  }, [range?.signature]);

  const blur = useCallback(() => {
    dismissed.current = "";
    setRange(null);
  }, []);

  return {
    range: scopeIsCurrent ? range : null,
    matches: scopeIsCurrent ? matches : [],
    loading: scopeIsCurrent && loading,
    loadError: scopeIsCurrent && loadError,
    hasErrors: scopeIsCurrent && !!catalog?.errors.length,
    updateRange,
    dismiss,
    blur,
    choose,
  };
}

export type SkillAutocompleteOptions = Parameters<
  typeof useSkillAutocompleteWithReader
>[0];
